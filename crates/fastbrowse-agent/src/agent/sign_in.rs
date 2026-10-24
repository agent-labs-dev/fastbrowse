//! `agent.py`'s `_shows_sign_in`: whether a page itself offers a sign-in, read from its controls.
//!
//! The three patterns are `_SIGN_IN_LABEL`, `_SIGN_IN_PROVIDER` and `_SIGN_IN_HREF`, all compiled with
//! `re.IGNORECASE`, built here as [`Pat`] trees so `\b`, `\s` and the case folding are Python's.

use std::sync::LazyLock;

use fastbrowse_core::page::{Control, Observation};
use fastbrowse_core::safety::asks_for_credentials;

use crate::pyre::{Pat, is_space};

/// One pattern character under `re.IGNORECASE`: sre folds an ASCII letter onto the few non-ASCII letters it also
/// matches, and any other cased letter onto its other case.
fn fold(want: char) -> Pat {
    if want.is_ascii_alphabetic() {
        return Pat::ilit(&want.to_string());
    }
    let lower = single(want.to_lowercase()).unwrap_or(want);
    Pat::class(move |c| c == want || single(c.to_lowercase()).unwrap_or(c) == lower)
}

/// A case mapping that is one character, as sre's per-character fold is.
fn single(mut mapped: impl Iterator<Item = char>) -> Option<char> {
    let first = mapped.next()?;
    mapped.next().is_none().then_some(first)
}

/// A literal under `re.IGNORECASE`. ASCII-only runs stay one [`Pat::ILit`].
fn ci(text: &str) -> Pat {
    if text.is_ascii() {
        return Pat::ilit(text);
    }
    Pat::Seq(text.chars().map(fold).collect())
}

/// `[\s-]?`
fn gap() -> Pat {
    Pat::opt(Pat::class(|c| is_space(c) || c == '-'))
}

/// `\s+`
fn spaces() -> Pat {
    Pat::plus(Pat::class(is_space))
}

/// `a[\s-]?b`
fn joined(a: &str, b: &str) -> Pat {
    Pat::Seq(vec![ci(a), gap(), ci(b)])
}

/// `a\s+b`
fn spaced(a: &str, b: &str) -> Pat {
    Pat::Seq(vec![ci(a), spaces(), ci(b)])
}

/// `[...]`, one of these characters under `re.IGNORECASE`.
fn either(chars: &str) -> Pat {
    Pat::alt(chars.chars().map(fold).collect())
}

fn words(words: &[&str]) -> Vec<Pat> {
    words.iter().map(|w| ci(w)).collect()
}

/// `\b(?:...)\b`
fn whole(options: Vec<Pat>) -> Pat {
    Pat::Seq(vec![Pat::Boundary, Pat::alt(options), Pat::Boundary])
}

/// The words a page puts on a control that offers a sign-in, in the languages sites are commonly written in.
static SIGN_IN_LABEL: LazyLock<Pat> = LazyLock::new(|| {
    let mut bounded = vec![joined("sign", "in"), joined("sign", "on"), joined("log", "in"), joined("log", "on")];
    bounded.extend(words(&["login", "logon", "authenticate", "authentication", "verify", "verification"]));
    bounded.extend([joined("two", "factor"), joined("one", "time")]);
    bounded.extend(words(&["sso", "saml", "oauth", "openid"]));
    bounded.extend([
        spaced("se", "connecter"),
        ci("connexion"),
        Pat::Seq(vec![ci("s"), Pat::opt(Pat::lit("'")), ci("identifier")]),
        Pat::Seq(vec![ci("s"), Pat::opt(Pat::lit("'")), ci("authentifier")]),
    ]);
    bounded.extend(words(&["anmelden", "anmeldung", "einloggen"]));
    bounded.extend([
        Pat::Seq(vec![ci("iniciar"), spaces(), ci("sesi"), either("oó"), ci("n")]),
        ci("acceder"),
        ci("conectarse"),
        Pat::Seq(vec![ci("iniciar"), spaces(), ci("sess"), either("aã"), ci("o")]),
        ci("acessar"),
    ]);
    bounded.extend(words(&["accedi", "accedere", "connettersi", "inloggen", "aanmelden"]));
    bounded.extend([
        spaced("logga", "in"),
        spaced("logg", "inn"),
        spaced("log", "ind"),
        Pat::Seq(vec![ci("zaloguj"), Pat::opt(Pat::Seq(vec![spaces(), ci("si"), either("eę")]))]),
        Pat::Seq(vec![ci("giri"), either("sş"), spaces(), ci("yap")]),
        ci("войти"),
        ci("вход"),
    ]);
    let mut options = vec![whole(bounded)];
    options.extend(words(&["ログイン", "サインイン", "登录", "登入", "로그인"]));
    options.extend([spaced("تسجيل", "الدخول"), spaced("साइन", "इन")]);
    Pat::alt(options)
});

/// A federated sign-in names its provider after a continuation word, and neither half says "sign in" alone.
static SIGN_IN_PROVIDER: LazyLock<Pat> = LazyLock::new(|| {
    let lead = ["continue", "proceed", "sign", "log", "mit", "avec", "con", "inloggen", "accedi", "entrar", "entra"];
    let provider = [
        "google",
        "github",
        "gitlab",
        "apple",
        "microsoft",
        "azure",
        "okta",
        "onelogin",
        "saml",
        "sso",
        "oauth",
        "openid",
        "facebook",
        "linkedin",
        "twitter",
        "slack",
        "passkey",
    ];
    let mut providers = words(&provider);
    // e-?mail
    providers.push(Pat::Seq(vec![ci("e"), Pat::opt(Pat::lit("-")), ci("mail")]));
    Pat::Seq(vec![whole(words(&lead)), Pat::repeat(Pat::class(|c| c != '\n'), 0, 24), whole(providers)])
});

/// A link to an authentication endpoint, which says the page wants a sign-in whatever its text is in.
static SIGN_IN_HREF: LazyLock<Pat> = LazyLock::new(|| {
    // `$` without MULTILINE: the end, or just before a newline that ends the text.
    let end = || {
        let at_end = || Pat::not_ahead(Pat::class(|_| true));
        Pat::alt(vec![at_end(), Pat::Ahead { pat: Box::new(Pat::Seq(vec![Pat::lit("\n"), at_end()])), negate: false }])
    };
    // `(?:[/?#]|$)`
    let stop = || Pat::alt(vec![Pat::class(|c| matches!(c, '/' | '?' | '#')), end()]);
    let ended = |path: &str| Pat::Seq(vec![ci(path), stop()]);
    let mut options = vec![ended("/sso"), ci("/saml"), ci("/oauth"), ci("/openid"), ci("/authorize"), ended("/auth")];
    // /log-?in and /sign-?in, each ended
    for (a, b) in [("/log", "in"), ("/sign", "in")] {
        options.push(Pat::Seq(vec![ci(a), Pat::opt(Pat::lit("-")), ci(b), stop()]));
    }
    options.extend(words(&[
        "/session/new",
        "/users/sign_in",
        "/adfs",
        "/keycloak",
        "/cognito",
        "accounts.google.com",
        "login.microsoftonline.com",
        "appleid.apple.com",
        "github.com/login",
        "facebook.com/dialog/oauth",
    ]));
    Pat::alt(options)
});

fn found(pattern: &Pat, text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    pattern.search(&chars, 0).is_some()
}

fn offers_sign_in(control: &Control) -> bool {
    if asks_for_credentials(control) {
        return true;
    }
    if found(&SIGN_IN_LABEL, &control.label) || found(&SIGN_IN_PROVIDER, &control.label) {
        return true;
    }
    control.href.as_deref().is_some_and(|href| !href.is_empty() && found(&SIGN_IN_HREF, href))
}

/// Whether the page itself offers a sign-in to act on: a credential field, or a control naming or linking one.
///
/// Absent controls are not evidence of missing authentication. A run that stopped on a page with neither a
/// credential field nor a sign-in affordance has not been shown a wall, so it is not reported as signed out. The
/// affordance is read from the label in the common languages of the web and from the link a control points at,
/// because a wall often offers only `Se connecter`, `Anmelden` or `Continue with Google` at `/sso`.
pub(crate) fn shows_sign_in(observation: &Observation) -> bool {
    observation.controls.iter().any(offers_sign_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_localized_or_federated_label_offers_a_sign_in() {
        for label in [
            "Sign in",
            "LOG-IN",
            "Se connecter",
            "Anmelden",
            "Iniciar sesión",
            "INICIAR SESIÓN",
            "Accedi",
            "ログイン",
            "Вход",
            "ВОЙТИ",
            "Zaloguj się",
            "Giriş yap",
            "Continue with SSO",
            "Continue with Google",
            "Sign up with e-mail",
        ] {
            assert!(found(&SIGN_IN_LABEL, label) || found(&SIGN_IN_PROVIDER, label), "{label}");
        }
    }

    #[test]
    fn a_word_that_merely_contains_a_sign_in_word_does_not() {
        for label in ["Menu", "Blogin", "Designing", "Account", "Continue", "Logo"] {
            assert!(!found(&SIGN_IN_LABEL, label) && !found(&SIGN_IN_PROVIDER, label), "{label}");
        }
    }

    #[test]
    fn a_link_to_an_authentication_endpoint_offers_a_sign_in_whatever_its_label() {
        for href in [
            "/sso",
            "/sso/start",
            "/SSO?next=/",
            "/login",
            "/log-in#top",
            "/auth\n",
            "https://accounts.google.com/o/oauth2/v2/auth",
            "/users/sign_in",
        ] {
            assert!(found(&SIGN_IN_HREF, href), "{href:?}");
        }
        for href in ["/ssounds", "/author/deep", "/logins", "/authority"] {
            assert!(!found(&SIGN_IN_HREF, href), "{href:?}");
        }
    }
}
