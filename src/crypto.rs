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
        // Переопределение нужно тестам, чтобы не трогать настоящий файл.
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
    /// Разобрать содержимое файла: `None` — пароль не подошёл (в отличие от
    /// пустого списка, который означает «аккаунтов пока нет»).
    pub(crate) fn load_accounts_with(content: &str, password: &str) -> Option<Vec<StoredAccount>> {
        if password.is_empty() {
            return None;
        }
        // Старый формат: незашифрованный список.
        if let Ok(v) = serde_json::from_str::<Vec<StoredAccount>>(content) {
            return Some(v);
        }
        Self::decrypt_accounts(content, password)
    }
    pub(crate) fn save_accounts(&self, password: &str) {
        if let Some(s) = Self::encrypt_accounts(&self.saved_accounts, password) {
            // Пишем во временный файл и переименовываем: если клиент убить
            // посреди записи, старый файл останется целым.
            let path = self.vault_path();
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    /// Файл хранилища конкретного экземпляра. Переопределение живёт в полях
    /// App, а не в переменной окружения: тесты идут параллельно, и общий
    /// env приводил к тому, что тест писал в настоящий файл.
    pub(crate) fn vault_path(&self) -> std::path::PathBuf {
        self.vault_path_override.clone().unwrap_or_else(Self::accounts_path)
    }
    /// Показать токен частично: первые и последние четыре символа.
    ///
    /// Режем по символам, а не по байтам. Раньше бралось `&token[..4]` и
    /// `&token[token.len() - 4..]`: токен берётся из поля ввода, и одна
    /// русская буква (2 байта) сдвигала границу — клиент падал с «byte index
    /// is not a char boundary». Маску показывают по клику на аккаунт, то есть
    /// достаточно было вставить токен с опечаткой и нажать на него.
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
    /// «изменён N мин назад» для файла хранилища, если он есть.
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
    /// Открыть хранилище паролем. Возвращает `Ok(None)` если всё чисто,
    /// `Ok(Some(подсказка))` если пароль подошёл после угадывания пробелов,
    /// `Err` если файл есть, а пароль не подошёл.
    ///
    /// Раньше здесь была ошибка: если аккаунтов в хранилище 0, но файл
    /// существует, `load_accounts` возвращал пустой вектор и мы показывали
    /// «Неверный пароль» даже с верным паролем.
    pub(crate) fn unlock_vault(&mut self, password: &str) -> Result<Option<String>, String> {
        if password.is_empty() {
            return Err("Введите пароль хранилища".to_string());
        }
        let content = match std::fs::read_to_string(self.vault_path()) {
            Ok(s) => s,
            Err(_) => {
                // Файла нет — это первый вход, создаём новое хранилище.
                self.saved_accounts = Vec::new();
                self.master_password = password.to_string();
                self.accounts_unlocked = true;
                self.refresh_active_index();
                return Ok(None);
            }
        };

        // Под пробелы/невидимые символы: их легко принести из буфера обмена
        // или случайно нажать пробел, а потом не вспомнить.
        for candidate in Self::password_variants(password) {
            let accounts = Self::load_accounts_with(&content, &candidate);
            if let Some(accounts) = accounts {
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
        }

        Err(format!(
            "Неверный пароль хранилища (файл {})",
            self.vault_path().display()
        ))
    }
    /// Набор вариантов пароля, которые пробуем подряд: как ввёл, без
    /// окружающих пробелов, и с лишним пробелом/переводом строки с любой
    /// стороны. Ошибка в один символ — самая частая причина «неверного пароля».
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
        // Как и при входе по токену: убираем выбор, чтобы при возврате на
        // экран входа показывалась обычная форма, а не форма аккаунта.
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

    /// Токен берётся из поля ввода, а маска резала строку по БАЙТАМ:
    /// `&token[..4]` и `&token[token.len() - 4..]`. Одна русская буква (2
    /// байта) сдвигает границу, и клиент падал с «byte index is not a char
    /// boundary». Падение происходило при клике на аккаунт, то есть
    /// достаточно было вставить токен с опечаткой и не заметить.
    #[test]
    fn mask_survives_non_ascii_token() {
        let a = app();
        let tokens = [
            "MTIzNDU2Nzg5MDEyMzQ1Njc4",       // обычный ASCII
            "ёаbсдеёфгhijклм",                   // кириллица в начале и в середине
            "abcdefghijКЛМНОП",                  // кириллица в конце
            "ЁЖЗИЙКЛМНОПРСТ",                    // только кириллица
            "токен-с-русскими-буквами-1234",
            // Три байта в начале: граница 4 байта попадает внутрь символа.
            "abc☺defghij",
            // Трёхбайтовые символы в конце: граница len-4 тоже попадает внутрь.
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
        // Короткий токен не показываем ни в каком виде, даже с русскими
        // буквами: маскировать нечего, а длина в символах, не в байтах.
        assert_eq!(a.mask_token("ёжик"), "••••••••");
        assert_eq!(a.mask_token(""), "••••••••");
    }

    /// То же через подпись аккаунта — это то место, где паника случалась на
    /// настоящем клике пользователя.
    #[test]
    fn account_label_survives_non_ascii_token() {
        let a = app();
        let acc = StoredAccount {
            token: "ёаbсдеёфгhijклм".to_string(),
            username: String::new(),
        };
        let label = a.account_label(&acc);
        assert!(label.contains('…'), "подпись должна быть замаскирована: {label:?}");
        // С известным именем токен не показывается вовсе — и это тоже должно
        // быть безопасно.
        let named = StoredAccount {
            token: "ёаbсдеёфгhijклм".to_string(),
            username: "Вася".to_string(),
        };
        assert_eq!(a.account_label(&named), "Вася");
    }
}
