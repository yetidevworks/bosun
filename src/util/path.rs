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
    expand_tilde_with(path, std::env::var("HOME").ok().as_deref())
}

/// The body of [`expand_tilde`] with `$HOME` passed in, so tests can
/// cover the unset and trailing-slash cases without touching the
/// process-global environment — mutating it would race any other test
/// that reads `HOME`, and several do (path shortening in the session
/// list, recents and quick-jump all read it).
fn expand_tilde_with(path: &str, home: Option<&str>) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    // `~` alone, or `~/anything`. Anything else is a `~user` form.
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_string();
    }
    let home = home.unwrap_or_default();
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

    #[test]
    fn expands_only_the_shell_forms() {
        let home = Some("/home/rhuk");
        assert_eq!(expand_tilde_with("~", home), "/home/rhuk");
        assert_eq!(expand_tilde_with("~/", home), "/home/rhuk/");
        assert_eq!(expand_tilde_with("~/work", home), "/home/rhuk/work");
        assert_eq!(
            expand_tilde_with("~/work/deep/er", home),
            "/home/rhuk/work/deep/er"
        );

        // Not tilde forms we resolve: absolute, relative, `~user`, and
        // a tilde that isn't leading.
        assert_eq!(expand_tilde_with("/abs/path", home), "/abs/path");
        assert_eq!(expand_tilde_with("relative/path", home), "relative/path");
        assert_eq!(expand_tilde_with("~other/work", home), "~other/work");
        assert_eq!(expand_tilde_with("/tmp/~/x", home), "/tmp/~/x");
        assert_eq!(expand_tilde_with("", home), "");
    }

    #[test]
    fn a_trailing_slash_on_home_does_not_double_up() {
        assert_eq!(expand_tilde_with("~/work", Some("/root/")), "/root/work");
        assert_eq!(expand_tilde_with("~", Some("/root/")), "/root");
    }

    #[test]
    fn without_a_home_the_path_is_left_alone() {
        // Better to hand tmux `~/work` unchanged (and have it fall back
        // to its own idea of home) than to rewrite it to `/work`.
        assert_eq!(expand_tilde_with("~/work", None), "~/work");
        assert_eq!(expand_tilde_with("~/work", Some("")), "~/work");
        assert_eq!(expand_tilde_with("~", None), "~");
    }

    /// The public wrapper reads the real `$HOME`; just check it agrees
    /// with the injected form rather than asserting on this machine's
    /// actual home directory.
    #[test]
    fn the_public_wrapper_uses_the_environment() {
        let home = std::env::var("HOME").ok();
        assert_eq!(
            expand_tilde("~/work"),
            expand_tilde_with("~/work", home.as_deref())
        );
    }
}
