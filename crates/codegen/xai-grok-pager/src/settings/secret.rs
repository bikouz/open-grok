//! Secret text owned by the settings UI.
//!
//! The wrapper deliberately never formats its contents and overwrites the
//! allocation before release. It is used only for transient credential input;
//! persisted settings continue to use [`super::SettingValue`].

use std::fmt;

use zeroize::Zeroize;

/// Transient secret input with redacted formatting and zeroize-on-drop.
#[derive(Default, PartialEq, Eq)]
pub struct SecretInput(String);

impl SecretInput {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub(crate) fn expose_mut(&mut self) -> &mut String {
        &mut self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretInput([REDACTED])")
    }
}

impl Drop for SecretInput {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_includes_secret() {
        let secret = SecretInput::new("sk-kimi-do-not-log".to_owned());
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "SecretInput([REDACTED])");
        assert!(!rendered.contains("sk-kimi"));
    }
}
