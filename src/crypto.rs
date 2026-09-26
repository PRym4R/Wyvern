use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::app::App;
use crate::models::StoredAccount;
use crate::util::{base64_decode, base64_string};

impl App {
    pub(crate) fn accounts_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            std::path::PathBuf::from(".wyvern_accounts.json")
        } else {
            std::path::Path::new(&home).join(".wyvern_accounts.json")
        }
    }
    pub(crate) fn derive_key(password: &str, salt: &[u8]) -> [u8; 32] {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, 100_000, &mut key);
        key
    }
    pub(crate) fn encrypt_accounts(accounts: &[StoredAccount], password: &str) -> Option<String> {
        if password.is_empty() {
            return None;
        }
        let mut salt = [0u8; 16];
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut salt);
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        let key = Self::derive_key(password, &salt);
        let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = serde_json::to_string(accounts).ok()?;
        let ct = cipher.encrypt(nonce, plaintext.as_bytes()).ok()?;

        Some(json!({
            "version": 1,
            "salt": base64_string(&salt),
            "nonce": base64_string(&nonce_bytes),
            "data": base64_string(&ct),
        }).to_string())
    }
    pub(crate) fn decrypt_accounts(content: &str, password: &str) -> Option<Vec<StoredAccount>> {
        if password.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(content).ok()?;
        let salt = base64_decode(&v["salt"].as_str()?.to_string())?;
        let nonce_bytes = base64_decode(&v["nonce"].as_str()?.to_string())?;
        let ct = base64_decode(&v["data"].as_str()?.to_string())?;

        let key = Self::derive_key(password, &salt);
        let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let pt = cipher.decrypt(nonce, ct.as_slice()).ok()?;
        serde_json::from_slice::<Vec<StoredAccount>>(&pt).ok()
    }
    pub(crate) fn load_accounts(password: &str) -> Vec<StoredAccount> {
        let path = Self::accounts_path();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Vec<StoredAccount>>(&s) {
                if password.is_empty() {
                    return Vec::new();
                }
                return v;
            }
            if let Some(accs) = Self::decrypt_accounts(&s, password) {
                return accs;
            }
        }
        Vec::new()
    }
    pub(crate) fn save_accounts(&self, password: &str) {
        if let Some(s) = Self::encrypt_accounts(&self.saved_accounts, password) {
            let _ = std::fs::write(Self::accounts_path(), s);
        }
    }
    pub(crate) fn mask_token(&self, token: &str) -> String {
        if token.len() <= 8 {
            return "••••••••".to_string();
        }
        let first = &token[..4];
        let last = &token[token.len() - 4..];
        format!("{}…{}", first, last)
    }
    pub(crate) fn add_saved_account(&mut self, token: &str, username: &str) {
        let token = token.trim().to_string();
        if token.is_empty() {
            return;
        }
        if let Some(a) = self.saved_accounts.iter_mut().find(|a| a.token == token) {
            if !username.is_empty() {
                a.username = username.to_string();
            }
        } else {
            self.saved_accounts.push(StoredAccount {
                token,
                username: username.to_string(),
            });
        }
        self.refresh_active_index();
        self.save_accounts(&self.master_password);
    }
    pub(crate) fn refresh_active_index(&mut self) {
        self.active_index = self.saved_accounts.iter().position(|a| a.token == self.token_input);
    }
    /// Имя для показа: username, если известен, иначе замаскированный токен.
    pub(crate) fn account_label(&self, acc: &StoredAccount) -> String {
        if acc.username.is_empty() {
            self.mask_token(&acc.token)
        } else {
            acc.username.clone()
        }
    }
    /// Открыть хранилище паролем. Пустой файл — считаем, что пароль верный
    /// (создаём новое хранилище), непустой файл, который не расшифровался, — ошибка.
    pub(crate) fn unlock_vault(&mut self, password: &str) -> Result<(), &'static str> {
        if password.is_empty() {
            return Err("Введите пароль хранилища");
        }
        let exists = Self::accounts_path().exists();
        let accounts = Self::load_accounts(password);
        if exists && accounts.is_empty() {
            return Err("Неверный пароль хранилища");
        }
        self.saved_accounts = accounts;
        self.master_password = password.to_string();
        self.accounts_unlocked = true;
        self.refresh_active_index();
        Ok(())
    }
    /// ЛКМ по аккаунту в нижней ленте: выбрать его и спросить пароль.
    pub(crate) fn select_account(&mut self, token: String) {
        self.login_selected = Some(token);
        self.status.clear();
        self.login_password = if self.accounts_unlocked {
            self.master_password.clone()
        } else {
            String::new()
        };
    }
    pub(crate) fn clear_login_selection(&mut self) {
        self.login_selected = None;
        self.login_password.clear();
        self.status.clear();
    }
    /// Войти в сохранённый аккаунт: пароль должен открыть хранилище,
    /// а токен — лежать в нём.
    pub(crate) fn login_with_password(&mut self, token: &str) {
        let pw = self.login_password.clone();
        if let Err(e) = self.unlock_vault(&pw) {
            self.status = e.to_string();
            return;
        }
        if !self.saved_accounts.iter().any(|a| a.token == token) {
            self.status = "Аккаунт не найден в хранилище".to_string();
            return;
        }
        self.login_password.clear();
        self.switch_account(token.to_string());
    }
}
