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
    ///
    /// Старый открытый формат здесь намеренно не разбирается: в нём пароля нет
    /// вовсе, и «подошёл любой» — это не проверка. См. `legacy_accounts` и
    /// `unlock_vault`.
    pub(crate) fn load_accounts_with(content: &str, password: &str) -> Option<Vec<StoredAccount>> {
        if password.is_empty() {
            return None;
        }
        Self::decrypt_accounts(content, password)
    }
    /// Старый формат хранилища — обычный список, без шифрования.
    ///
    /// Такой файл открывается без пароля (так он и был записан), и раньше это
    /// считалось «пароль подошёл». Опасность не в том, что файл открыт — он и
    /// так открыт, — а в том, что происходило следом: `unlock_vault` запоминал
    /// введённый пароль как пароль хранилища, а первая же запись (добавили
    /// аккаунт, обновили имя) перешифровывала файл этим паролем. Опечатка при
    /// вводе — и хранилище молча переезжало на пароль с опечаткой, вернуть
    /// прежний уже нечем, и никакого «миграция старого формата» никто не видел.
    pub(crate) fn legacy_accounts(content: &str) -> Option<Vec<StoredAccount>> {
        serde_json::from_str::<Vec<StoredAccount>>(content).ok()
    }
    pub(crate) fn save_accounts(&self, password: &str) {
        // Старый открытый файл сам себя не перезаписывает. Пароль в нём не
        // проверялся, поэтому запись перешифровала бы хранилище тем, что
        // случайно оказалось в поле ввода, — и доступ к своим аккаунтам был бы
        // потерян молча. Пока пользователь не подтвердил пароль явно
        // (`confirm_legacy_migration`), файл не трогаем.
        if self.vault_legacy {
            return;
        }
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
                self.vault_legacy = false;
                self.refresh_active_index();
                return Ok(None);
            }
        };

        // Старый открытый формат: пароля в файле нет, поэтому «подошёл» любой
        // ввод. Впускаем (данные и так открыты), но помечаем хранилище как
        // требующее подтверждения: до него файл не перезаписывается, иначе
        // опечатка в пароле молча закрыла бы хранилище не тем паролем.
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

        // Под пробелы/невидимые символы: их легко принести из буфера обмена
        // или случайно нажать пробел, а потом не вспомнить.
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
    /// Набор вариантов пароля, которые пробуем: как ввёл, без окружающих
    /// пробелов, и с лишним пробелом или переводом строки с любой стороны.
    /// Ошибка в один символ — самая частая причина «неверного пароля».
    ///
    /// Считаются они не подряд, а одновременно — см. `try_variants`. Сам список
    /// трогать нельзя: лишний пробел в пароле хранилища, с которым человек уже
    /// работает, обязан продолжать подходить.
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
    /// Подобрать вариант пароля по содержимому файла.
    ///
    /// Каждый вариант — это отдельный ключ, то есть 100 000 итераций PBKDF2:
    /// на этой машине 8.6 мс, на слабом компьютере в разы больше. Варианты
    /// считали подряд, и неверный пароль — а это самый частый случай неудачного
    /// входа — стоил восьми таких вычислений подряд: 71 мс здесь и сотни
    /// миллисекунд на слабом компьютере, всё это время в потоке интерфейса, где
    /// окно не отвечает. Повторялось на каждое нажатие «Войти».
    ///
    /// Теперь первый вариант — как ввёл — считается на месте (в подавляющем
    /// большинстве случаев он и есть верный, и потоки не нужны вовсе), а
    /// остальные разом, каждый в своём потоке: варианты друг от друга не
    /// зависят, и ждать приходится самый долгий, а не сумму. Работы столько же,
    /// но окно отвечает.
    ///
    /// Дёшево отвергнуть варианты нельзя: что-нибудь вроде быстрого хеша пароля
    /// рядом с данными превратило бы файл в то, что перебирается со скоростью
    /// SHA-256 вместо PBKDF2. Пусть лучше подождёт.
    ///
    /// Возвращает номер подошедшего варианта: он нужен, чтобы положить в
    /// хранилище именно тот пароль, которым файл на самом деле открылся.
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
            // Из подошедших берём вариант с наименьшим номером, иначе выбор
            // зависел бы от того, кто из потоков успел раньше.
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
    /// Закрепить пароль за старым открытым хранилищем.
    ///
    /// Вызывается только по явному нажатию: до него файл не перезаписывается
    /// (см. `save_accounts`), потому что пароль в старом формате никак не
    /// проверялся и опечатка в нём закрыла бы хранилище не тем паролем молча.
    pub(crate) fn confirm_legacy_migration(&mut self) {
        if !self.vault_legacy {
            return;
        }
        self.vault_legacy = false;
        let pw = self.master_password.clone();
        self.save_accounts(&pw);
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

/// Замок на тесты, которые много считают ключи.
///
/// Перебор вариантов пароля — это сотни тысяч итераций PBKDF2 на вариант, и
/// тесты идут параллельно. Без замка тяжёлые тесты мешают друг другу.
#[cfg(test)]
pub(crate) static VAULT_COST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Сколько потоков поднял перебор вариантов пароля в последний раз — только для
// тестов. Потоковый, а не общий: тесты идут параллельно, и общий счётчик
// показывал бы чужой перебор — тогда проверка верного пароля падала бы из-за
// соседнего теста. Значение ставит тот поток, который вызвал `try_variants`,
// поэтому потокового и достаточно.
#[cfg(test)]
thread_local! {
    static LAST_SEARCH_WIDTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Сколько потоков поднял перебор вариантов пароля в последний раз.
///
/// По секундомеру параллельность проверить нельзя: на загруженной машине
/// (а машина разработчика вполне может быть занята игрой) запас между
/// последовательным перебором и параллельным слишком мал, и проверка мигала бы
/// то так, то этак. Счётчик потоков отвечает на тот же вопрос точно.
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

    /// Каждый тест хранилища — свой файл: тесты идут параллельно.
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

    /// Перебрать варианты подряд — ровно как было. Нужен как эталон для
    /// проверки, что перебор действительно распараллелен.
    fn sweep_sequentially(content: &str, variants: &[String]) -> Option<(usize, Vec<StoredAccount>)> {
        for (i, candidate) in variants.iter().enumerate() {
            if let Some(accounts) = App::load_accounts_with(content, candidate) {
                return Some((i, accounts));
            }
        }
        None
    }

    /// Неверный пароль не должен заставлять ждать все варианты подряд.
    ///
    /// Сценарий самый частый: человек ошибся в пароле хранилища. Каждый
    /// вариант — отдельный ключ, то есть 100 000 итераций PBKDF2 (здесь 8.6 мс,
    /// на слабом компьютере в разы больше), а вариантов восемь. Считали их
    /// подряд, и всё это время окно интерфейса не отвечало — 71 мс на этой
    /// машине и сотни миллисекунд на слабом, на каждое нажатие «Войти».
    ///
    /// Проверяем не по секундомеру, а по числу поднятых потоков: время на
    /// загруженной машине шумит так, что запас между последовательным и
    /// параллельным перебором уходит в разброс. Потоки же говорят прямо:
    /// варианты считаются разом.
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

    /// Верный пароль — самый частый случай — не должен поднимать ни одного
    /// потока: он подходит с первого варианта, и лишняя работа тут не нужна.
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

    /// Перебор не должен терять то, ради чего он и есть: пароль хранилища с
    /// лишним пробелом должен открываться по обрезанному варианту, а с
    /// пробелом спереди — по варианту «пробел + как ввёл», который теперь
    /// считается не первым, а в отдельном потоке.
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

    /// Введённый с пробелами пароль обязан подойти: человек лишний раз нажал
    /// пробел, а хранилище должно открыться, а не отшивать его «неверным
    /// паролем».
    #[test]
    fn typed_with_spaces_opens_the_vault() {
        let (mut app, tmp) = vaulted();
        app.save_accounts("правильный");
        let hint = app.unlock_vault("  правильный \n").expect("пароль с пробелами должен подойти");
        assert!(hint.is_some(), "обрезка должна сопровождаться подсказкой");
        assert_eq!(app.master_password, "правильный");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Перебор состоит из семи-восьми вариантов: обрезанный добавляется только
    /// когда он отличается от введённого. Сокращать список нельзя: лишний
    /// пробел в пароле, с которым человек уже работает, обязан продолжать
    /// подходить.
    #[test]
    fn variants_keep_the_whitespace_guesses() {
        let variants = App::password_variants("пароль");
        assert_eq!(variants[0], "пароль", "первым идёт как ввёл — самый частый случай");
        assert_eq!(
            variants,
            vec!["пароль", "пароль ", " пароль", "пароль\n", "\nпароль", "пароль\r\n", "\r\nпароль"]
        );
        // С пробелами по краям добавляется обрезанный вариант.
        let padded = App::password_variants(" пароль ");
        assert_eq!(padded[1], "пароль", "обрезанный идёт вторым");
        assert_eq!(padded.len(), variants.len() + 1);
    }
}

/// Старый открытый формат хранилища: пароля в файле нет, и это не значит, что
/// «подошёл любой пароль».
#[cfg(test)]
mod legacy_vault_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    static TAG: AtomicU64 = AtomicU64::new(0);

    /// Экземпляр клиента со своим файлом хранилища: тесты идут параллельно.
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

    /// Содержимое старого формата: обычный список без шифрования.
    fn legacy_contents() -> String {
        let accounts = vec![StoredAccount { token: "старый-токен".into(), username: "вася".into() }];
        serde_json::to_string(&accounts).unwrap()
    }

    /// Открытый список — это не «пароль подошёл».
    ///
    /// Раньше `load_accounts_with` разбирал его до всякой проверки пароля и
    /// отдавал аккаунты. Дальше `unlock_vault` запоминал введённый пароль как
    /// пароль хранилища, и первая же запись перешифровывала файл этим паролем:
    /// опечатка при вводе — и доступ к своим аккаунтам потерян молча, без
    /// всякого «миграция старого формата».
    #[test]
    fn plaintext_list_is_not_a_password_check() {
        let contents = legacy_contents();
        for password in ["любой", "другой", "", "любой "] {
            assert!(
                App::load_accounts_with(&contents, password).is_none(),
                "открытый список не должен открываться паролем {password:?}: пароля в нём нет"
            );
        }
        // Но разобрать его как старый формат можно — и это отдельный ответ.
        assert_eq!(App::legacy_accounts(&contents).map(|a| a.len()), Some(1));
        assert!(App::legacy_accounts("{\"salt\":\"x\"}").is_none());
    }

    /// До подтверждения старый файл не перезаписывается: иначе опечатка в
    /// пароле молча закрыла бы хранилище не тем паролем.
    #[test]
    fn legacy_vault_is_not_rewritten_before_confirmation() {
        let contents = legacy_contents();
        let (mut app, tmp) = app_with_file(Some(&contents));

        let notice = app.unlock_vault("опечатка").expect("старый формат должен открываться");
        assert!(notice.is_some(), "пользователю надо сказать про старый формат");
        assert!(app.vault_legacy, "хранилище должно быть помечено как требующее подтверждения");
        assert_eq!(app.saved_accounts.len(), 1, "аккаунты из старого файла должны быть видны");

        // Любая обычная запись (добавили аккаунт, обновили имя) файл не трогает.
        app.add_saved_account("новый-токен", "петя");
        app.save_accounts("опечатка");
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            contents,
            "старый открытый файл нельзя перешифровывать, пока пароль не подтверждён"
        );

        // Явное подтверждение — и файл переезжает в зашифрованный формат.
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

    /// Обычное зашифрованное хранилище миграция не задевает: флаг не встаёт, и
    /// запись идёт как раньше.
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

    /// Неверный пароль к зашифрованному файлу по-прежнему отклоняется, и это не
    /// путается со старым форматом.
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
