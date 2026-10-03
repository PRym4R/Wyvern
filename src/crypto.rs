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
mod mask_tests {
    use super::*;
    use tokio::sync::mpsc;

    fn app() -> App {
        let (_, rx) = mpsc::unbounded_channel();
        App::new(rx)
    }

    /// The mask used to slice by bytes and panicked on multi-byte tokens.
    #[test]
    fn mask_survives_non_ascii_token() {
        let a = app();
        let tokens = [
            "MTIzNDU2Nzg5MDEyMzQ1Njc4",       // plain ASCII
            "ёаbсдеёфгhijклм",                   // Cyrillic at the start and middle
            "abcdefghijКЛМНОП",                  // Cyrillic at the end
            "ЁЖЗИЙКЛМНОПРСТ",                    // Cyrillic only
            "токен-с-русскими-буквами-1234",
            // Three bytes at the start: the 4-byte boundary lands inside a char.
            "abc☺defghij",
            // Three-byte chars at the end: the len-4 boundary lands inside one too.
            "abcdefghабв",
        ];
        for t in tokens {
            let chars: Vec<char> = t.chars().collect();
            let expected = if chars.len() <= 8 {
                "••••••••".to_string()
            } else {
                format!(
                    "{}…{}",
                    chars[..4].iter().collect::<String>(),
                    chars[chars.len() - 4..].iter().collect::<String>()
                )
            };
            assert_eq!(a.mask_token(t), expected, "токен {t:?}");
        }
        // Short tokens are never shown, even with Cyrillic; length counts chars, not bytes.
        assert_eq!(a.mask_token("ёжик"), "••••••••");
        assert_eq!(a.mask_token(""), "••••••••");
    }

    /// Same check through the account label, where the panic hit a real click.
    #[test]
    fn account_label_survives_non_ascii_token() {
        let a = app();
        let acc = StoredAccount {
            token: "ёаbсдеёфгhijклм".to_string(),
            username: String::new(),
        };
        let label = a.account_label(&acc);
        assert!(label.contains('…'), "подпись должна быть замаскирована: {label:?}");
        // With a known username the token isn't shown at all; that must be safe too.
        let named = StoredAccount {
            token: "ёаbсдеёфгhijклм".to_string(),
            username: "Вася".to_string(),
        };
        assert_eq!(a.account_label(&named), "Вася");
    }
}

/// Serializes key-heavy tests; parallel password sweeps would interfere.
#[cfg(test)]
pub(crate) static VAULT_COST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Thread count of the last password sweep, for tests. Thread-local so parallel
// tests don't observe each other's sweeps.
#[cfg(test)]
thread_local! {
    static LAST_SEARCH_WIDTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Thread count of the last password sweep.
/// Counts threads instead of timing, since wall-clock deltas are too noisy.
#[cfg(test)]
pub(crate) fn last_search_width() -> usize {
    LAST_SEARCH_WIDTH.with(|n| n.get())
}

#[cfg(test)]
fn set_last_search_width(width: usize) {
    LAST_SEARCH_WIDTH.with(|n| n.set(width));
}

#[cfg(test)]
mod vault_cost_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    /// Each vault test uses its own file since tests run in parallel.
    static TAG: AtomicU64 = AtomicU64::new(0);

    fn vaulted() -> (App, std::path::PathBuf) {
        let tag = TAG.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("wyvern-test-cost-{}-{}.json", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.vault_path_override = Some(p.clone());
        (app, p)
    }

    /// Sequential variant sweep, as a baseline to check the parallel one.
    fn sweep_sequentially(content: &str, variants: &[String]) -> Option<(usize, Vec<StoredAccount>)> {
        for (i, candidate) in variants.iter().enumerate() {
            if let Some(accounts) = App::load_accounts_with(content, candidate) {
                return Some((i, accounts));
            }
        }
        None
    }

    /// A wrong password must search all variants in parallel, not sequentially.
    /// Verified by thread count rather than timing.
    #[test]
    fn wrong_password_searches_the_variants_in_parallel() {
        let _guard = super::VAULT_COST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut app, tmp) = vaulted();
        app.save_accounts("правильный");
        let content = std::fs::read_to_string(&tmp).expect("файл хранилища не записался");
        let variants = App::password_variants(" неправильный ");
        assert_eq!(variants.len(), 8, "перебор состоит из восьми вариантов");
        assert!(
            sweep_sequentially(&content, &variants).is_none(),
            "неверный пароль не должен подходить"
        );

        assert!(app.unlock_vault(" неправильный ").is_err(), "с неверным паролем вход отклоняется");
        assert_eq!(
            super::last_search_width(),
            variants.len() - 1,
            "семь оставшихся вариантов должны считаться разом: подряд это 71 мс ожидания в потоке интерфейса"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// A correct password (the common case) must spawn no threads.
    #[test]
    fn correct_password_needs_no_threads() {
        let _guard = super::VAULT_COST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut app, tmp) = vaulted();
        app.save_accounts("правильный");
        assert!(app.unlock_vault("правильный").is_ok());
        assert_eq!(
            super::last_search_width(),
            0,
            "первый вариант подошёл — потоки не нужны"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// Re-adding an existing account must not re-encrypt the vault.
    #[test]
    fn adding_an_existing_account_does_not_rewrite_the_vault() {
        let _guard = super::VAULT_COST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut app, tmp) = vaulted();
        app.master_password = "правильный".into();
        app.saved_accounts = vec![StoredAccount {
            token: "токен".into(),
            username: "вася".into(),
        }];
        app.save_accounts("правильный");
        let before = std::fs::read_to_string(&tmp).expect("файл хранилища не записался");

        // Logging into an existing account: empty name, nothing to change.
        app.add_saved_account("токен", "");

        let after = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(
            after, before,
            "хранилище перешифровано без изменений — лишний PBKDF2 на входе"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// Stray whitespace in the stored password must still open the vault.
    #[test]
    fn stray_whitespace_still_opens_the_vault() {
        for saved in ["правильный", "правильный ", " правильный", "правильный\n"] {
            let (mut app, tmp) = vaulted();
            app.saved_accounts = vec![StoredAccount { token: "токен".into(), username: "вася".into() }];
            app.save_accounts(saved);
            let hint = app
                .unlock_vault("правильный")
                .unwrap_or_else(|e| panic!("пароль хранилища {saved:?} должен открываться: {e}"));
            if saved == "правильный" {
                assert!(hint.is_none(), "для точного пароля подсказки быть не должно");
            } else {
                assert!(hint.is_some(), "пароль {saved:?} принят не как введён — нужна подсказка");
            }
            assert_eq!(
                app.master_password, saved,
                "в хранилище должен лежать тот пароль, которым файл и открылся"
            );
            assert_eq!(app.saved_accounts.len(), 1, "аккаунт не загрузился");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// A password typed with extra spaces must still open the vault.
    #[test]
    fn typed_with_spaces_opens_the_vault() {
        let (mut app, tmp) = vaulted();
        app.save_accounts("правильный");
        let hint = app.unlock_vault("  правильный \n").expect("пароль с пробелами должен подойти");
        assert!(hint.is_some(), "обрезка должна сопровождаться подсказкой");
        assert_eq!(app.master_password, "правильный");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Sweep has 7-8 variants; the trimmed one is added only when it differs.
    #[test]
    fn variants_keep_the_whitespace_guesses() {
        let variants = App::password_variants("пароль");
        assert_eq!(variants[0], "пароль", "первым идёт как ввёл — самый частый случай");
        assert_eq!(
            variants,
            vec!["пароль", "пароль ", " пароль", "пароль\n", "\nпароль", "пароль\r\n", "\r\nпароль"]
        );
        // With surrounding spaces, the trimmed variant is added.
        let padded = App::password_variants(" пароль ");
        assert_eq!(padded[1], "пароль", "обрезанный идёт вторым");
        assert_eq!(padded.len(), variants.len() + 1);
    }
}

/// Legacy plaintext vault: no password in the file, so any input isn't a match.
#[cfg(test)]
mod legacy_vault_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    static TAG: AtomicU64 = AtomicU64::new(0);

    /// A client instance with its own vault file since tests run in parallel.
    fn app_with_file(contents: Option<&str>) -> (App, std::path::PathBuf) {
        let tag = TAG.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("wyvern-test-legacy-{}-{}.json", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        if let Some(c) = contents {
            std::fs::write(&p, c).unwrap();
        }
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.vault_path_override = Some(p.clone());
        (app, p)
    }

    /// Legacy format contents: a plain unencrypted list.
    fn legacy_contents() -> String {
        let accounts = vec![StoredAccount { token: "старый-токен".into(), username: "вася".into() }];
        serde_json::to_string(&accounts).unwrap()
    }

    /// A plaintext list must not be treated as a password match.
    #[test]
    fn plaintext_list_is_not_a_password_check() {
        let contents = legacy_contents();
        for password in ["любой", "другой", "", "любой "] {
            assert!(
                App::load_accounts_with(&contents, password).is_none(),
                "открытый список не должен открываться паролем {password:?}: пароля в нём нет"
            );
        }
        // But it can be parsed as legacy format, which is a separate answer.
        assert_eq!(App::legacy_accounts(&contents).map(|a| a.len()), Some(1));
        assert!(App::legacy_accounts("{\"salt\":\"x\"}").is_none());
    }

    /// A legacy file isn't rewritten before confirmation.
    #[test]
    fn legacy_vault_is_not_rewritten_before_confirmation() {
        let contents = legacy_contents();
        let (mut app, tmp) = app_with_file(Some(&contents));

        let notice = app.unlock_vault("опечатка").expect("старый формат должен открываться");
        assert!(notice.is_some(), "пользователю надо сказать про старый формат");
        assert!(app.vault_legacy, "хранилище должно быть помечено как требующее подтверждения");
        assert_eq!(app.saved_accounts.len(), 1, "аккаунты из старого файла должны быть видны");

        // Ordinary writes (add account, update name) leave the legacy file alone.
        app.add_saved_account("новый-токен", "петя");
        app.save_accounts("опечатка");
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            contents,
            "старый открытый файл нельзя перешифровывать, пока пароль не подтверждён"
        );

        // Explicit confirmation migrates the file to the encrypted format.
        app.confirm_legacy_migration();
        assert!(!app.vault_legacy, "после подтверждения ждать больше нечего");
        let after = std::fs::read_to_string(&tmp).unwrap();
        assert_ne!(after, contents, "после подтверждения файл должен быть перезаписан");
        assert!(after.contains("salt") && after.contains("nonce"), "файл должен быть зашифрован");
        assert_eq!(App::load_accounts_with(&after, "опечатка").map(|a| a.len()), Some(2));
        assert!(
            App::load_accounts_with(&after, "правильный").is_none(),
            "хранилище закрыто именно введённым паролем — тем, который подтвердили"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// An encrypted vault is untouched by the legacy path.
    #[test]
    fn encrypted_vault_is_untouched_by_the_legacy_path() {
        let (mut app, tmp) = app_with_file(None);
        app.saved_accounts = vec![StoredAccount { token: "т".into(), username: "вася".into() }];
        app.save_accounts("правильный");

        assert!(app.unlock_vault("правильный").is_ok());
        assert!(!app.vault_legacy, "зашифрованный файл — не старый формат");
        app.add_saved_account("ещё-токен", "петя");
        assert_eq!(
            App::load_accounts_with(&std::fs::read_to_string(&tmp).unwrap(), "правильный")
                .map(|a| a.len()),
            Some(2),
            "новый аккаунт должен сохраниться"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// A wrong password on an encrypted file is still rejected, not misread as legacy.
    #[test]
    fn encrypted_vault_still_rejects_a_wrong_password() {
        let (mut app, tmp) = app_with_file(None);
        app.save_accounts("правильный");
        assert!(app.unlock_vault("неправильный").is_err());
        assert!(!app.vault_legacy);
        assert!(app.unlock_vault("правильный").is_ok());
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod vault_write_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    static TAG: AtomicU64 = AtomicU64::new(0);

    /// Own file per test since tests run in parallel.
    fn vaulted() -> (App, std::path::PathBuf) {
        let tag = TAG.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("wyvern-test-write-{}-{}.json", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(&p);
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.vault_path_override = Some(p.clone());
        (app, p)
    }

    fn one_account(app: &mut App) {
        app.saved_accounts = vec![StoredAccount { token: "секрет".into(), username: "вася".into() }];
    }

    /// The vault file must be owner-only since it holds tokens.
    #[cfg(unix)]
    #[test]
    fn vault_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, tmp) = vaulted();
        one_account(&mut app);
        app.save_accounts("правильный");
        let mode = std::fs::metadata(&tmp)
            .expect("файл хранилища не записался")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "хранилище должно быть только для владельца, а не {:o}",
            mode & 0o777
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// A failed write must surface in the status, not silently pretend success.
    #[test]
    fn vault_write_error_is_reported_in_status() {
        let (mut app, tmp) = vaulted();
        let missing = std::env::temp_dir()
            .join(format!("wyvern-no-such-dir-{}", std::process::id()))
            .join("accounts.json");
        let _ = std::fs::remove_dir_all(missing.parent().unwrap());
        app.vault_path_override = Some(missing);
        one_account(&mut app);
        app.save_accounts("правильный");
        assert!(
            app.status.contains("хранилищ"),
            "ошибка записи должна попасть в статус, а там: {:?}",
            app.status
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// A failed rename (a directory at the vault path) must also be reported.
    #[test]
    fn vault_rename_error_is_reported_and_tmp_is_cleaned() {
        let (mut app, dir) = vaulted();
        let _ = std::fs::remove_file(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        one_account(&mut app);
        app.save_accounts("правильный");
        assert!(
            app.status.contains("хранилищ"),
            "ошибка замены файла должна попасть в статус, а там: {:?}",
            app.status
        );
        assert!(
            !dir.with_extension("tmp").exists(),
            "временный файл должен быть убран"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another client instance pointing at the same vault file.
    fn app_at(path: &std::path::Path) -> App {
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = App::new(rx);
        app.vault_path_override = Some(path.to_path_buf());
        app
    }

    /// A second instance must not open the same vault, or two gateways clobber
    /// one account's writes; the lock releases when the first instance drops.
    #[test]
    fn second_instance_cannot_open_the_same_vault() {
        let (mut first, tmp) = vaulted();
        one_account(&mut first);
        first.save_accounts("правильный");
        assert!(
            first.unlock_vault("правильный").is_ok(),
            "первый экземпляр должен открыть своё хранилище"
        );

        let mut second = app_at(&tmp);
        let err = second
            .unlock_vault("правильный")
            .expect_err("второй экземпляр не должен открыть занятое хранилище");
        assert!(
            err.contains("другом экземпляре"),
            "нужен понятный отказ, а не {err:?}"
        );

        // First instance dropped, lock released, access possible again.
        drop(first);
        let mut third = app_at(&tmp);
        assert!(
            third.unlock_vault("правильный").is_ok(),
            "после выхода замок должен освобождаться"
        );

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("lock"));
    }
}
