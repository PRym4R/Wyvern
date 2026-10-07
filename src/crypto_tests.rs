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
