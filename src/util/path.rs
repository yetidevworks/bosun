//! Path helpers for user-typed filesystem paths.
//!
//! Bosun accepts paths the way a shell prompt does — including a
//! leading `~` — but the things it hands those paths to don't do
//! shell expansion. `tmux new-session -c '~/work'` doesn't resolve the
//! tilde; worse, when the directory doesn't exist tmux silently starts
//! the session in `$HOME` instead, so a session the user asked for in
//! `~/work` quietly lands in `~`. `git -C` behaves the same way. So we
//! expand the tilde ourselves before a path leaves bosun.

/// Expand a leading `~` or `~/…` to `$HOME`.
///
/// Returns the input unchanged when there's nothing to do: no leading
/// tilde, a `~user` form (which only a shell can resolve, and which we
/// deliberately don't guess at), or an unset/empty `$HOME` — in that
/// last case the original path is still more useful than a string that
/// silently rewrites to an absolute `/…`.
pub fn expand_tilde(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    // `~` alone, or `~/anything`. Anything else is a `~user` form.
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return path.to_string();
    }
    // `~` -> `$HOME`; `~/work` -> `$HOME/work`. Trim a trailing slash
    // on HOME so `HOME=/root/` doesn't produce `/root//work`.
    let home = home.trim_end_matches('/');
    if rest.is_empty() {
        return home.to_string();
    }
    format!("{home}{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `std::env::set_var` is process-global, so these run under one
    /// test fn rather than racing each other across threads.
    #[test]
    fn expands_only_the_shell_forms() {
        let saved = std::env::var("HOME").ok();
        // SAFETY: single-threaded within this test; restored below.
        unsafe { std::env::set_var("HOME", "/home/rhuk") };

        assert_eq!(expand_tilde("~"), "/home/rhuk");
        assert_eq!(expand_tilde("~/"), "/home/rhuk/");
        assert_eq!(expand_tilde("~/work"), "/home/rhuk/work");
        assert_eq!(expand_tilde("~/work/deep/er"), "/home/rhuk/work/deep/er");

        // Not tilde forms we resolve: absolute, relative, `~user`, and
        // a tilde that isn't leading.
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
        assert_eq!(expand_tilde("~other/work"), "~other/work");
        assert_eq!(expand_tilde("/tmp/~/x"), "/tmp/~/x");
        assert_eq!(expand_tilde(""), "");

        // A trailing slash on HOME must not double up.
        unsafe { std::env::set_var("HOME", "/root/") };
        assert_eq!(expand_tilde("~/work"), "/root/work");
        assert_eq!(expand_tilde("~"), "/root");

        // No HOME to expand against: leave the path alone rather than
        // rewriting `~/work` to `/work`.
        unsafe { std::env::remove_var("HOME") };
        assert_eq!(expand_tilde("~/work"), "~/work");
        assert_eq!(expand_tilde("~"), "~");

        match saved {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
