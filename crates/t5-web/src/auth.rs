//! 登录与令牌。
//!
//! - **口令**：argon2id 散列（PHC 字符串）存进 `config.toml` 的 `[web]` 段；
//! - **令牌**：自实现的 HS256 JWT（`hmac` + `sha2`），不引入 `ring` ——
//!   后者在 musl 静态构建下需要额外配置 C 工具链，而这里只需要一个
//!   标准库之外的纯 Rust 实现即可；
//! - **初始凭据**：首次启动生成随机密码，打印到日志一次，并置
//!   `must_change_password`，未改密前只能访问改密接口；
//! - **限速**：按来源 IP 记录失败次数，超过阈值后指数退避锁定。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use t5_core::logbuf::now_secs;
use t5_core::Controller;

type HmacSha256 = Hmac<Sha256>;

/// 初始密码长度（不含分隔符）。约 92 bit 熵，足以对抗在线爆破。
const INITIAL_PASSWORD_LEN: usize = 16;
/// 触发锁定所需的连续失败次数。
const MAX_FAILURES: u32 = 5;
/// 首次触发的锁定时长（秒），之后按失败次数翻倍。
const LOCK_BASE_SECS: u64 = 60;
/// 锁定时长上限（秒）。
const LOCK_MAX_SECS: u64 = 900;
/// 新密码长度限制。
const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 128;

/// JWT 载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// 用户名
    pub sub: String,
    /// 签发时间（Unix 秒）
    pub iat: u64,
    /// 过期时间（Unix 秒）
    pub exp: u64,
    /// 是否仍处于「必须先改密」状态
    #[serde(default)]
    pub pwd: bool,
}

/// 登录结果，直接作为 `/api/auth/login` 的响应。
#[derive(Debug, Clone, Serialize)]
pub struct LoginOutcome {
    pub token: String,
    pub username: String,
    /// 为真时前端必须先跳到改密页
    pub must_change_password: bool,
    /// 令牌有效期（秒）
    pub expires_in: u64,
}

struct FailRecord {
    count: u32,
    until: u64,
}

/// 认证器。持有 JWT 密钥缓存与失败计数。
pub struct Authenticator {
    ctrl: Controller,
    secret: Vec<u8>,
    failures: Mutex<HashMap<IpAddr, FailRecord>>,
}

impl Authenticator {
    /// 初始化：补齐缺失的口令散列与 JWT 密钥并写回配置。
    ///
    /// 首次运行时生成的初始密码会以 WARN 级别写入日志（仅此一次），
    /// 使用者需要从 `t5d` 的标准输出或日志页里取走它。
    pub async fn init(ctrl: Controller) -> Result<Arc<Self>, String> {
        let (username, mut password_hash, mut jwt_secret) = {
            let cfg = ctrl.config().await;
            (
                cfg.web.username.clone(),
                cfg.web.password_hash.clone(),
                cfg.web.jwt_secret.clone(),
            )
        };

        let mut dirty = false;
        // 只有「本次生成了初始密码」时才强制改密。
        // 若仅补发 JWT 密钥同样进入 dirty 分支，但绝不能重置该标志，
        // 否则每次重启都会把已改过密码的用户再赶去改密页。
        let mut generated_password = false;

        if password_hash.trim().is_empty() {
            let password = random_password(INITIAL_PASSWORD_LEN);
            password_hash = hash_password(&password)?;
            generated_password = true;
            ctrl.logs
                .warn("Web 控制台已初始化，以下是初始凭据（仅显示这一次）");
            ctrl.logs.warn(format!("  用户名：{username}"));
            ctrl.logs.warn(format!("  初始密码：{password}"));
            ctrl.logs
                .warn("  首次登录后必须修改密码；请立即妥善保存或完成改密");
            dirty = true;
        }

        if jwt_secret.trim().is_empty() {
            jwt_secret = random_secret();
            dirty = true;
        }

        if dirty {
            let mut cfg = ctrl.cfg.lock().await;
            cfg.web.username = username.clone();
            cfg.web.password_hash = password_hash.clone();
            cfg.web.jwt_secret = jwt_secret.clone();
            if generated_password {
                cfg.web.must_change_password = true;
            }
            cfg.save(ctrl.config_path.as_path())
                .map_err(|e| format!("保存 Web 凭据失败: {e}"))?;
        }

        Ok(Arc::new(Self {
            ctrl,
            secret: jwt_secret.into_bytes(),
            failures: Mutex::new(HashMap::new()),
        }))
    }

    pub fn username(&self) -> String {
        // 用户名可能被改密流程改写，这里统一读配置
        self.ctrl
            .cfg
            .try_lock()
            .map(|c| c.web.username.clone())
            .unwrap_or_else(|_| "admin".to_string())
    }

    // ---------------- 令牌 ----------------

    /// 签发令牌。
    pub fn issue(&self, username: &str, must_change: bool, ttl_secs: u64) -> String {
        let now = now_secs();
        let claims = Claims {
            sub: username.to_string(),
            iat: now,
            exp: now + ttl_secs.max(60),
            pwd: must_change,
        };
        self.sign(&claims)
    }

    fn sign(&self, claims: &Claims) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = serde_json::to_vec(claims)
            .map(|v| URL_SAFE_NO_PAD.encode(v))
            .unwrap_or_default();
        let signing_input = format!("{header}.{payload}");

        let signature = match HmacSha256::new_from_slice(&self.secret) {
            Ok(mut mac) => {
                mac.update(signing_input.as_bytes());
                URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
            }
            Err(_) => String::new(),
        };

        format!("{signing_input}.{signature}")
    }

    /// 校验令牌；签名不符、格式错误或已过期都返回 `None`。
    pub fn verify(&self, token: &str) -> Option<Claims> {
        let mut parts = token.split('.');
        let (header, payload, signature) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }

        let signing_input = format!("{header}.{payload}");
        let raw_sig = URL_SAFE_NO_PAD.decode(signature).ok()?;

        let mut mac = HmacSha256::new_from_slice(&self.secret).ok()?;
        mac.update(signing_input.as_bytes());
        // 常数时间比较，避免通过响应时间逐字节猜测签名
        mac.verify_slice(&raw_sig).ok()?;

        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        if claims.exp <= now_secs() {
            return None;
        }
        Some(claims)
    }

    // ---------------- 登录 ----------------

    /// 校验凭据并签发令牌。失败时限速。
    pub async fn login(
        &self,
        ip: IpAddr,
        username: &str,
        password: &str,
    ) -> Result<LoginOutcome, String> {
        let locked = self.locked_secs(ip).await;
        if locked > 0 {
            return Err(format!("尝试次数过多，请 {locked} 秒后再试"));
        }

        let cfg = self.ctrl.config().await;
        let user_ok = constant_eq(username.trim(), cfg.web.username.trim());
        let password_ok = verify_password(password, &cfg.web.password_hash);

        if !(user_ok && password_ok) {
            let remains = self.record_failure(ip).await;
            self.ctrl
                .logs
                .warn(format!("Web 控制台登录失败（来源 {ip}，剩余尝试 {remains} 次）"));
            return Err("用户名或密码不正确".into());
        }

        self.clear_failures(ip).await;
        let ttl = ttl_secs(&cfg.web.token_ttl_hours);
        self.ctrl
            .logs
            .info(format!("Web 控制台登录成功（来源 {ip}）"));

        Ok(LoginOutcome {
            token: self.issue(&cfg.web.username, cfg.web.must_change_password, ttl),
            username: cfg.web.username.clone(),
            must_change_password: cfg.web.must_change_password,
            expires_in: ttl,
        })
    }

    /// 修改密码（首次改密与日常改密走同一条路径）。返回新令牌。
    pub async fn change_password(
        &self,
        claims: &Claims,
        new_password: &str,
    ) -> Result<String, String> {
        let password = new_password.trim();
        if password.len() < MIN_PASSWORD_LEN {
            return Err(format!("新密码至少 {MIN_PASSWORD_LEN} 位"));
        }
        if password.len() > MAX_PASSWORD_LEN {
            return Err("新密码过长".into());
        }
        if password.chars().all(|c| c.is_ascii_digit()) {
            return Err("新密码不能是纯数字".into());
        }

        let hash = hash_password(password)?;
        let ttl = {
            let mut cfg = self.ctrl.cfg.lock().await;
            cfg.web.password_hash = hash;
            cfg.web.must_change_password = false;
            cfg.save(self.ctrl.config_path.as_path())
                .map_err(|e| format!("保存配置失败: {e}"))?;
            ttl_secs(&cfg.web.token_ttl_hours)
        };

        self.ctrl.logs.info("Web 控制台密码已更新");
        Ok(self.issue(&claims.sub, false, ttl))
    }

    // ---------------- 限速 ----------------

    async fn locked_secs(&self, ip: IpAddr) -> u64 {
        let now = now_secs();
        let guard = self.failures.lock().await;
        match guard.get(&ip) {
            Some(r) if r.until > now && r.count >= MAX_FAILURES => r.until - now,
            _ => 0,
        }
    }

    /// 记一次失败，返回还剩余多少次尝试机会（0 表示已锁定）。
    async fn record_failure(&self, ip: IpAddr) -> u32 {
        let now = now_secs();
        let mut guard = self.failures.lock().await;
        let rec = guard.entry(ip).or_insert(FailRecord { count: 0, until: 0 });

        // 锁定期已过则重新计数
        if rec.until <= now {
            rec.count = 0;
            rec.until = 0;
        }

        rec.count += 1;
        if rec.count >= MAX_FAILURES {
            let over = (rec.count - MAX_FAILURES).min(4) as u32;
            rec.until = now + LOCK_BASE_SECS.saturating_mul(1 << over).min(LOCK_MAX_SECS);
            0
        } else {
            MAX_FAILURES - rec.count
        }
    }

    async fn clear_failures(&self, ip: IpAddr) {
        let mut guard = self.failures.lock().await;
        guard.remove(&ip);
    }
}

fn ttl_secs(hours: &u64) -> u64 {
    // 允许 1 分钟 ~ 30 天，避免配置写错导致令牌永久有效
    hours.clamp(1, 24 * 30) * 3600
}

/// 用 argon2id 生成 PHC 字符串。
fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::encode_b64(&random_bytes(16))
        .map_err(|e| format!("生成盐失败: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("口令散列失败: {e}"))
}

/// 校验口令；散列串损坏时按失败处理。
fn verify_password(password: &str, phc: &str) -> bool {
    if phc.trim().is_empty() {
        return false;
    }
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// 常数时间字符串比较，避免用户名比对泄漏长度/前缀信息。
fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// 32 字节随机密钥，用 base64url 表示。
fn random_secret() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(32))
}

/// 生成易读的随机初始密码：每 4 位用 `-` 分隔，便于人工抄写。
fn random_password(len: usize) -> String {
    // 去掉 0/O/1/l/I 等易混字符
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(len + len / 4);
    for i in 0..len {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        let idx = (rng.next_u32() as usize) % ALPHABET.len();
        out.push(ALPHABET[idx] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_verifies_round_trip() {
        let phc = hash_password("hunter2hunter2").unwrap();
        assert!(phc.starts_with("$argon2"));
        assert!(verify_password("hunter2hunter2", &phc));
        assert!(!verify_password("wrong-password", &phc));
    }

    #[test]
    fn rejects_broken_hash_string() {
        assert!(!verify_password("whatever", ""));
        assert!(!verify_password("whatever", "not-a-phc-string"));
    }

    #[test]
    fn constant_eq_matches_std_eq() {
        assert!(constant_eq("admin", "admin"));
        assert!(!constant_eq("admin", "admix"));
        assert!(!constant_eq("admin", "admin "));
    }

    #[test]
    fn random_password_shape_and_alphabet() {
        let p = random_password(16);
        let digits = p.chars().filter(|c| c.is_ascii_alphanumeric()).count();
        assert_eq!(digits, 16);
        assert_eq!(p.matches('-').count(), 3);
        for c in p.chars().filter(|c| *c != '-') {
            assert!("abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(c));
        }
        assert_ne!(random_password(16), random_password(16));
    }

    #[test]
    fn ttl_is_clamped() {
        assert_eq!(ttl_secs(&0), 3600);
        assert_eq!(ttl_secs(&24), 86_400);
        assert_eq!(ttl_secs(&9999), 24 * 30 * 3600);
    }
}
