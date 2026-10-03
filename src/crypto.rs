use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::app::App;
use crate::models::StoredAccount;
use crate::util::{base64_decode, base64_string};

/// Write the vault atomically via a 0600 temp file, fsync, and rename.
fn write_vault_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // Don't leave a stray temp file next to the vault.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl App {
    pub(crate) fn accounts_path() -> std::path::PathBuf {
        // Override for tests so the real file isn't touched.
        if let Ok(custom) = std::env::var("WYVERN_ACCOUNTS_PATH") {
            if !custom.is_empty() {
                return std::path::PathBuf::from(custom);
            }
        }
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
    /// Parse the file; `None` means the password didn't match (vs. an empty list).
    pub(crate) fn load_accounts_with(content: &str, password: &str) -> Option<Vec<StoredAccount>> {
        if password.is_empty() {
            return None;
        }
        Self::decrypt_accounts(content, password)
    }
    /// Legacy vault format: a plain unencrypted list.
    /// Treating it as a password match would let a typo silently re-encrypt it.
    pub(crate) fn legacy_accounts(content: &str) -> Option<Vec<StoredAccount>> {
        serde_json::from_str::<Vec<StoredAccount>>(content).ok()
    }
    pub(crate) fn save_accounts(&mut self, password: &str) {
        // Don't rewrite a legacy file until the password is explicitly confirmed.
        if self.vault_legacy {
            return;
        }
        let Some(encrypted) = Self::encrypt_accounts(&self.saved_accounts, password) else {
            return;
        };
        // Don't swallow write errors: the UI would claim success while the disk is stale.
        if let Err(e) = write_vault_file(&self.vault_path(), encrypted.as_bytes()) {
            self.status = format!("Не удалось сохранить хранилище: {}", e);
        }
    }
    /// This instance's vault file; the override lives in App fields because
    /// parallel tests shared one env var and wrote the real file.
    pub(crate) fn vault_path(&self) -> std::path::PathBuf {
        self.vault_path_override.clone().unwrap_or_else(Self::accounts_path)
    }
    /// Path to the lock file next to the vault: `~/.wyvern_accounts.lock`.
    pub(crate) fn vault_lock_path(&self) -> std::path::PathBuf {
        self.vault_path().with_extension("lock")
    }
    /// Take the vault lock for the whole session; a second instance gets an error.
    /// A write-only lock wouldn't separate two clients racing on one account.
    pub(crate) fn acquire_vault_lock(&mut self) -> Result<(), String> {
        if self.vault_lock.is_some() {
            return Ok(());
        }
        let path = self.vault_lock_path();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("Не удалось открыть замок хранилища ({}): {}", path.display(), e))?;
        match file.try_lock() {
            Ok(()) => {
                self.vault_lock = Some(file);
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                Err("Хранилище уже открыто в другом экземпляре клиента".to_string())
            }
            Err(std::fs::TryLockError::Error(e)) => {
                Err(format!("Не удалось заблокировать хранилище: {}", e))
            }
        }
    }
    /// Show the first and last four chars of the token.
    /// Slice by chars, not bytes: a multi-byte token would panic on a byte boundary.
    pub(crate) fn mask_token(&self, token: &str) -> String {
        let chars: Vec<char> = token.chars().collect();
        if chars.len() <= 8 {
            return "••••••••".to_string();
        }
        let first: String = chars[..4].iter().collect();
        let last: String = chars[chars.len() - 4..].iter().collect();
        format!("{}…{}", first, last)
    }
    pub(crate) fn add_saved_account(&mut self, token: &str, username: &str) {
        let token = token.trim().to_string();
        if token.is_empty() {
            return;
        }
        let mut changed = false;
        if let Some(a) = self.saved_accounts.iter_mut().find(|a| a.token == token) {
            if !username.is_empty() && a.username != username {
                a.username = username.to_string();
                changed = true;
            }
        } else {
            self.saved_accounts.push(StoredAccount {
                token,
                username: username.to_string(),
            });
            changed = true;
        }
        self.refresh_active_index();
        // Only save when the list changed; unconditional saves wasted a full PBKDF2 run.
        if changed {
            let pw = self.master_password.clone();
            self.save_accounts(&pw);
        }
    }
    pub(crate) fn refresh_active_index(&mut self) {
        self.active_index = self.saved_accounts.iter().position(|a| a.token == self.token_input);
    }
    /// Display name: username if known, otherwise the masked token.
    pub(crate) fn account_label(&self, acc: &StoredAccount) -> String {
        if acc.username.is_empty() {
            self.mask_token(&acc.token)
        } else {
            acc.username.clone()
        }
    }
    /// "modified N ago" text for the vault file, if present.
    pub(crate) fn vault_age_text(&self) -> Option<String> {
        let meta = std::fs::metadata(self.vault_path()).ok()?;
        let secs = meta.modified().ok()?.elapsed().ok()?.as_secs();
        Some(if secs < 90 {
            format!("изменён {} сек назад", secs)
        } else if secs < 5400 {
            format!("изменён {} мин назад", secs / 60)
        } else {
            format!("изменён {} ч назад", secs / 3600)
        })
    }
    /// Unlock the vault. `Ok(Some(hint))` if a whitespace-guessed variant matched.
    /// An empty stored account list must still count as a successful unlock.
    pub(crate) fn unlock_vault(&mut self, password: &str) -> Result<Option<String>, String> {
        if password.is_empty() {
            return Err("Введите пароль хранилища".to_string());
        }
        // Take the lock before reading so no second instance can write over it.
        self.acquire_vault_lock()?;
        let content = match std::fs::read_to_string(self.vault_path()) {
            Ok(s) => s,
            Err(_) => {
                // No file: first run, create a new vault.
                self.saved_accounts = Vec::new();
                self.master_password = password.to_string();
                self.accounts_unlocked = true;
                self.vault_legacy = false;
                self.refresh_active_index();
                return Ok(None);
            }
        };

        // Legacy plaintext format: admit the data, but require confirmation before
        // rewriting, or a mistyped password would silently lock the vault.
        if let Some(accounts) = Self::legacy_accounts(&content) {
            self.saved_accounts = accounts;
            self.master_password = password.to_string();
            self.accounts_unlocked = true;
            self.vault_legacy = true;
            self.refresh_active_index();
            return Ok(Some(
                "Хранилище старого формата: в нём нет пароля. Пароль, который ты ввёл, станет \
                 паролем хранилища только после подтверждения — нажми «Закрепить пароль»."
                    .to_string(),
            ));
        }

        // Try whitespace variants: they're easy to paste or press by accident.
        let variants = Self::password_variants(password);
        if let Some((idx, accounts)) = Self::try_variants(&content, &variants) {
            let candidate = variants[idx].clone();
            let hint = if candidate == password {
                None
            } else {
                Some("Пароль принят: убрал лишние пробелы".to_string())
            };
            self.saved_accounts = accounts;
            self.master_password = candidate;
            self.accounts_unlocked = true;
            self.refresh_active_index();
            return Ok(hint);
        }

        Err(format!(
            "Неверный пароль хранилища (файл {})",
            self.vault_path().display()
        ))
    }
    /// Password variants to try: as typed, trimmed, and with stray whitespace
    /// on either side. The list is fixed; `try_variants` runs them in parallel.
    fn password_variants(password: &str) -> Vec<String> {
        let mut out = vec![password.to_string()];
        let trimmed = password.trim();
        if trimmed.len() != password.len() {
            out.push(trimmed.to_string());
        }
        for extra in [" ", "\n", "\r\n"] {
            out.push(format!("{}{}", password, extra));
            out.push(format!("{}{}", extra, password));
        }
        out
    }
    /// Find the password variant that matches the file contents.
    /// The first (as-typed) variant is tried inline; the rest run in parallel,
    /// since each costs a full PBKDF2 pass and a wrong password is common.
    /// Returns the matching variant index so the exact password is stored.
    fn try_variants(content: &str, variants: &[String]) -> Option<(usize, Vec<StoredAccount>)> {
        #[cfg(test)]
        set_last_search_width(0);
        let first = variants.first()?;
        if let Some(accounts) = Self::load_accounts_with(content, first) {
            return Some((0, accounts));
        }
        if variants.len() == 1 {
            return None;
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = variants
                .iter()
                .enumerate()
                .skip(1)
                .map(|(i, candidate)| {
                    scope.spawn(move || Self::load_accounts_with(content, candidate).map(|acc| (i, acc)))
                })
                .collect();
            #[cfg(test)]
            set_last_search_width(handles.len());
            // Take the lowest-index match so the result doesn't depend on thread timing.
            let mut best: Option<(usize, Vec<StoredAccount>)> = None;
            for handle in handles {
                if let Ok(Some(found)) = handle.join() {
                    if best.as_ref().is_none_or(|(b, _)| found.0 < *b) {
                        best = Some(found);
                    }
                }
            }
            best
        })
    }
    /// Pin the password to a legacy plaintext vault.
    /// Only called on explicit confirmation; until then the file isn't rewritten.
    pub(crate) fn confirm_legacy_migration(&mut self) {
        if !self.vault_legacy {
            return;
        }
        self.vault_legacy = false;
        let pw = self.master_password.clone();
        self.save_accounts(&pw);
    }
    /// Left-click an account in the bottom bar: select it and ask for the password.
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
    /// Log into a saved account: the password must unlock the vault and the
    /// token must be stored in it.
    pub(crate) fn login_with_password(&mut self, token: &str) {
        let pw = self.login_password.clone();
        self.login_notice.clear();
        match self.unlock_vault(&pw) {
            Ok(hint) => {
                if let Some(h) = hint {
                    self.login_notice = h;
                }
            }
            Err(e) => {
                self.status = e;
                return;
            }
        }
        if !self.saved_accounts.iter().any(|a| a.token == token) {
            self.status = "Аккаунт не найден в хранилище".to_string();
            return;
        }
        // Clear the selection so returning to the login screen shows the plain form.
        self.login_selected = None;
        self.login_password.clear();
        self.switch_account(token.to_string());
    }
}

#[cfg(test)]
include!("crypto_tests.rs");
