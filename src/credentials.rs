use anyhow::{Context, Result, ensure};
use keyring::Entry;

const SERVICE: &str = "App Stream";

pub fn load_or_prompt(username: &str) -> Result<String> {
    let entry =
        Entry::new(SERVICE, username).context("Opening Windows Credential Manager entry")?;
    let password = resolve(&entry, || crate::gui::password_prompt(username))?;
    Ok(password)
}

pub fn delete(username: &str) -> Result<()> {
    let entry =
        Entry::new(SERVICE, username).context("Opening Windows Credential Manager entry")?;
    match entry.delete_credential() {
        Ok(()) => eprintln!("Saved RTSP password deleted for username {username:?}."),
        Err(keyring::Error::NoEntry) => {
            eprintln!("No saved RTSP password for username {username:?}.")
        }
        Err(error) => {
            return Err(error).context("Deleting RTSP password from Windows Credential Manager");
        }
    }
    Ok(())
}

trait PasswordStore {
    fn get(&self) -> Result<Option<String>>;
    fn set(&self, password: &str) -> Result<()>;
}

impl PasswordStore for Entry {
    fn get(&self) -> Result<Option<String>> {
        match self.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => {
                Err(error).context("Reading RTSP password from Windows Credential Manager")
            }
        }
    }

    fn set(&self, password: &str) -> Result<()> {
        self.set_password(password)
            .context("Saving RTSP password in Windows Credential Manager")
    }
}

fn resolve(store: &impl PasswordStore, prompt: impl FnOnce() -> Result<String>) -> Result<String> {
    if let Some(password) = store.get()? {
        validate(&password)?;
        return Ok(password);
    }
    let password = prompt()?;
    validate(&password)?;
    store.set(&password)?;
    Ok(password)
}

fn validate(password: &str) -> Result<()> {
    ensure!(
        !password.is_empty(),
        "RTSP password must not be empty; use --delete-password to clear a saved entry"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct Store {
        value: RefCell<Option<String>>,
        fail_read: bool,
        fail_write: bool,
        writes: Cell<u32>,
    }

    impl PasswordStore for Store {
        fn get(&self) -> Result<Option<String>> {
            ensure!(!self.fail_read, "Credential store unavailable");
            Ok(self.value.borrow().clone())
        }

        fn set(&self, password: &str) -> Result<()> {
            ensure!(!self.fail_write, "Credential store write failed");
            self.writes.set(self.writes.get() + 1);
            self.value.replace(Some(password.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn prompts_once_then_reuses_password_without_trimming() {
        let store = Store::default();
        let password = resolve(&store, || Ok("  exact password  ".into())).unwrap();
        assert_eq!(password, "  exact password  ");
        assert_eq!(
            resolve(&store, || panic!("Must reuse saved password")).unwrap(),
            password
        );
        assert_eq!(store.writes.get(), 1);
    }

    #[test]
    fn saved_password_is_not_overwritten() {
        let store = Store {
            value: RefCell::new(Some("old".into())),
            ..Store::default()
        };
        assert_eq!(
            resolve(&store, || panic!("Must not prompt")).unwrap(),
            "old"
        );
        assert_eq!(store.value.borrow().as_deref(), Some("old"));
        assert_eq!(store.writes.get(), 0);
    }

    #[test]
    fn rejects_empty_passwords_without_saving() {
        let store = Store::default();
        assert!(resolve(&store, || Ok(String::new())).is_err());
        assert_eq!(store.writes.get(), 0);
    }

    #[test]
    fn storage_failures_are_explicit_and_never_fall_back_to_prompt() {
        let store = Store {
            fail_read: true,
            ..Store::default()
        };
        assert!(resolve(&store, || panic!("Read failure must not prompt")).is_err());
        let store = Store {
            fail_write: true,
            ..Store::default()
        };
        assert!(resolve(&store, || Ok("test".into())).is_err());
    }

    #[test]
    fn prompt_errors_do_not_save() {
        let store = Store::default();
        assert!(resolve(&store, || anyhow::bail!("Prompt cancelled")).is_err());
        assert_eq!(store.writes.get(), 0);
    }

    #[test]
    #[ignore = "writes and deletes its own uniquely named Windows Credential Manager entry"]
    fn windows_credential_manager_round_trip_is_isolated_per_username() {
        let username = format!("app-stream-test-{:032x}", rand::random::<u128>());
        let other_username = format!("{username}-other");
        let entry = Entry::new(SERVICE, &username).unwrap();
        struct Cleanup(Entry);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                match self.0.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(error) => eprintln!("Cleaning up test credential failed: {error}"),
                }
            }
        }
        let _cleanup = Cleanup(Entry::new(SERVICE, &username).unwrap());
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
        assert_eq!(
            resolve(&entry, || Ok("  integration-only  ".into())).unwrap(),
            "  integration-only  "
        );
        assert_eq!(load_or_prompt(&username).unwrap(), "  integration-only  ");
        assert!(matches!(
            Entry::new(SERVICE, &other_username).unwrap().get_password(),
            Err(keyring::Error::NoEntry)
        ));
        delete(&username).unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
        assert_eq!(
            resolve(&entry, || Ok("updated-test-only".into())).unwrap(),
            "updated-test-only"
        );
        assert_eq!(load_or_prompt(&username).unwrap(), "updated-test-only");
        delete(&username).unwrap();
        delete(&username).unwrap();
    }
}
