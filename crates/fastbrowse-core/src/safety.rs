//! Authorization for irreversible actions, and secrets that never leave their origins or reach logs.
//!
//! Models only ever see secret names. Values are resolved here, at dispatch time, for the origin being acted on.
//! Origins are read as Chrome reads them (WHATWG, ADR 0002); the cases where that differs from `urlsplit` are pinned
//! in the tests.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::Value;
use url::Url;

use crate::BoxFuture;
use crate::decision::NoulQuestion;
use crate::models::{Operation, SecretRef, SecretResolver, SecretValue, UNTRUSTED, WallKind};
use crate::page::Control;
use crate::py::json::write_string;
use crate::py::text;

/// Whether Jev is asked before this dispatches: every click, drag and Enter.
///
/// Whether a click commits is a judgment about the page, so Jev makes it rather than a word list. A list of committing
/// labels is never complete ("Place your order", "Yes, I'm sure"), and a link commits as easily as a button: a
/// one-click unsubscribe is an href, and a script handler runs whatever the element says.
pub fn may_be_irreversible(operation: Operation) -> bool {
    match operation {
        Operation::Click | Operation::Drag => true,
        // Not only a form's Enter: a chat, comment or DM box sends on Enter through its own script, with no form for
        // the page to describe.
        Operation::Enter => true,
        // A dialog's accept and a caller's address are asked about where they are handled; the rest change nothing
        // off the page.
        Operation::Navigate
        | Operation::Hover
        | Operation::Fill
        | Operation::Select
        | Operation::Escape
        | Operation::Scroll
        | Operation::ScrollUp
        | Operation::Back
        | Operation::SwitchTab
        | Operation::Upload
        | Operation::Dialog
        | Operation::Read
        | Operation::Done
        | Operation::Escalate => false,
    }
}

/// What tells this control apart from another with the same label: where it sits, what it drops onto, the form it
/// submits and the address it links to.
fn describe_control(instructions: &mut String, operation: Operation, control: &Control, destination: Option<&Control>) {
    if let Some(context) = control.context.as_deref().filter(|c| !c.is_empty()) {
        instructions.push_str(&format!("It sits under {} on the page.\n", text::repr(context)));
    }
    if let Some(destination) = destination {
        instructions.push_str(&format!("Drop it onto the element labelled {}.\n", text::repr(&destination.label)));
        if let Some(context) = destination.context.as_deref().filter(|c| !c.is_empty()) {
            instructions.push_str(&format!("The drop target sits under {} on the page.\n", text::repr(context)));
        }
    }
    if let Some(semantics) = control.submit_semantics.as_deref().filter(|s| !s.is_empty())
        && (operation == Operation::Enter || (operation == Operation::Click && control.role == "button"))
    {
        instructions.push_str(&format!("The action submits this form: {semantics}\n"));
    }
    // Without the address, a settings link labelled "Change your password" read as the change itself.
    if let Some(href) = control.href.as_deref().filter(|href| !href.is_empty() && *href != control.label)
        && operation == Operation::Click
    {
        instructions.push_str(&format!("It is a link to {href}\n"));
    }
}

pub fn irreversible_question(
    task: &str,
    operation: Operation,
    control: &Control,
    destination: Option<&Control>,
) -> NoulQuestion {
    let mut instructions = format!(
        "{UNTRUSTED}\nThe agent is about to {operation} the element labelled {} while doing this task: {task}\n",
        text::repr(&control.label)
    );
    describe_control(&mut instructions, operation, control, destination);
    instructions.push_str(
        "Would doing so commit something that cannot be undone, such as placing an order or spending money, sending a \
         message, submitting an application, changing or resetting an account password or credentials, or deleting or \
         publishing data?",
    );
    NoulQuestion::new(
        instructions,
        Some("It commits an irreversible or externally visible change."),
        Some("It only navigates, filters, reveals or edits a draft that can still be changed."),
    )
}

/// What the agent is about to do to a control, with what tells it apart from another with the same label.
///
/// A submission also says what the form's other fields hold, from `controls`: the form's own text leaves out typed
/// values, and a form that reads as empty does not read as the message the task asked to send. A masked field is
/// left out.
pub fn control_action(operation: Operation, control: &Control, controls: &[Control]) -> String {
    let mut action = format!("{operation} the element labelled {}.\n", text::repr(&control.label));
    describe_control(&mut action, operation, control, None);
    let submits = operation == Operation::Enter || (operation == Operation::Click && control.role == "button");
    if submits && control.form_id.is_some() {
        let held: Vec<String> = controls
            .iter()
            .filter(|c| c.id != control.id && !c.sensitive)
            .filter(|c| (&c.frame_id, &c.form_id) == (&control.frame_id, &control.form_id))
            .filter_map(|c| {
                let value = c.value.as_deref().filter(|v| !v.is_empty())?;
                Some(format!("{} holds {}", text::repr(&c.label), text::repr(value)))
            })
            .collect();
        if !held.is_empty() {
            action.push_str(&format!("In the form, {}.\n", held.join("; ")));
        }
    }
    action
}

/// Whether the task asks for this commit, asked beside [`irreversible_question`] when the run is authorized.
///
/// `action` is what the agent is about to do, as [`control_action`] writes it for a control. A task that lists its
/// steps in order ("place it with this postcode, and if it is rejected, with that one") asks for its first step too:
/// asked only whether the task asked for this, Jev put that first order at 0.27, and with the steps named at 0.87 or
/// more, while commits a task forbade or never asked for stayed at 0.24 or less.
pub fn requested_question(task: &str, action: &str) -> NoulQuestion {
    NoulQuestion::new(
        format!(
            "{UNTRUSTED}\nThe task: {task}\nThe agent is about to {action}Does the task ask the agent to do this, as \
             the task itself or as one of the steps it lists in order? A step the task asks for counts even when a \
             later step redoes or corrects it."
        ),
        Some("The task asks for this action."),
        Some("The task asks for something else."),
    )
}

// File types a browser saves rather than shows. Chrome renders a page, an image or a PDF; an archive, an installer, a
// disk image or a spreadsheet goes to the downloads folder whatever the link that leads to it says.
const DOWNLOADED_SUFFIXES: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "exe", "msi", "dmg", "pkg", "deb", "rpm", "apk",
    "appimage", "iso", "img", "jar", "torrent", "csv", "tsv", "xls", "xlsx", "ods", "doc", "docx", "odt", "ppt",
    "pptx", "odp",
];

/// Whether the page says activating this control saves a file, before anything is clicked.
///
/// The page's own evidence, not the label: a `download` attribute, or a link to a file type a browser saves. A link's
/// address is the one thing an icon-only download button carries, so it holds up where a label says nothing or says
/// it in another language.
pub fn starts_download(control: &Control) -> bool {
    if control.download == Some(true) {
        return true;
    }
    let Some(href) = control.href.as_deref().filter(|h| !h.is_empty()) else { return false };
    // A relative href is read against a placeholder base: only its path matters here.
    let base = Url::parse("https://base.invalid/").expect("a constant address parses");
    let Ok(parsed) = base.join(href) else { return false };
    let name = parsed.path().rsplit('/').next().unwrap_or("");
    name.rsplit_once('.').is_some_and(|(_, suffix)| DOWNLOADED_SUFFIXES.contains(&suffix.to_lowercase().as_str()))
}

/// Whether the task rules out downloading: asked of the task alone, once a control would download a file.
pub fn downloads_question(task: &str) -> NoulQuestion {
    NoulQuestion::new(
        format!(
            "The agent is doing this task: {task}\nDoes the task rule out downloading files, either by telling the \
             agent not to download anything or by saying the work is read-only?"
        ),
        Some("The task says not to download files, or calls the work read-only."),
        Some("The task says nothing against downloading, even if it only asks for information."),
    )
}

/// Whether the page declares this a password field. `sensitive` is no evidence: it also marks a text field holding a
/// secret the agent typed, and a sign-in form whose username took a stored secret read as two password fields.
pub fn is_password(control: &Control) -> bool {
    control.input_type.as_deref() == Some("password")
}

fn autocompletes(control: &Control, token: &str) -> bool {
    let tokens = control.autocomplete.as_deref().unwrap_or("").to_lowercase();
    text::split(&tokens).any(|t| t == token)
}

/// The password fields of `control`'s form, in page order; None for a control outside any form.
fn form_passwords<'a>(controls: &'a [Control], control: &Control) -> Option<Vec<&'a Control>> {
    control.form_id.as_ref()?;
    let same_form = |c: &&Control| (&c.frame_id, &c.form_id) == (&control.frame_id, &control.form_id);
    Some(controls.iter().filter(same_form).filter(|c| is_password(c)).collect())
}

/// Whether a password field takes a replacement password, on the page's own structure rather than its wording.
///
/// Only a form holding more than one password field sets one: the current password and a new one, or a new one and
/// its confirmation. There `autocomplete` says which is which when the page marks it, and otherwise the first takes
/// the current password and the rest the new one. The only password field of a form takes the account's password
/// whatever its `autocomplete` says: sign-in pages mark their one field `new-password` to keep browsers from filling
/// it. None when the structure cannot tell: a field outside any form on a page with other password fields, which
/// must not be given the stored current password on a guess.
pub fn sets_new_password(controls: &[Control], field: &Control) -> Option<bool> {
    let passwords = form_passwords(controls, field);
    if passwords.as_ref().is_some_and(|passwords| passwords.len() < 2) {
        return Some(false);
    }
    if autocompletes(field, "new-password") {
        return Some(true);
    }
    if autocompletes(field, "current-password") {
        return Some(false);
    }
    match passwords {
        Some(passwords) => passwords.iter().position(|c| c.id == field.id).map(|at| at > 0),
        None => (!controls.iter().any(|c| is_password(c) && c.id != field.id)).then_some(false),
    }
}

/// Whether this step submits a form that changes an account's password: an Enter in, or a button of, a form holding
/// more than one password field. A link or a field's focus never is.
pub fn changes_credentials(controls: &[Control], target: &Control, operation: Operation) -> bool {
    let submits = match operation {
        Operation::Enter => true,
        Operation::Click => target.role == "button" && target.href.as_deref().is_none_or(str::is_empty),
        _ => false,
    };
    submits && form_passwords(controls, target).is_some_and(|passwords| passwords.len() > 1)
}

// Tokens a page uses to mark the fields of a sign-in form, whatever language its labels are in.
const SIGN_IN_AUTOCOMPLETE: &[&str] = &["username", "current-password", "one-time-code", "webauthn"];

/// Which part of a sign-in a credential field takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialField {
    /// A password or another secret the page masks.
    Password,
    /// A code the site sends for this sign-in (`autocomplete=one-time-code`).
    OneTimeCode,
    /// The account's name (`autocomplete=username`).
    Username,
    /// A passkey prompt (`autocomplete=webauthn`).
    Passkey,
}

/// What credential a field takes, on the page's own evidence that it wants a sign-in; None for any other field.
///
/// A one-time code is read first: a site masks its code box as it masks a password, and a caller told "password"
/// for a code it could read from an email asks the wrong person for the wrong thing.
pub fn credential_field(control: &Control) -> Option<CredentialField> {
    let tokens = control.autocomplete.as_deref().unwrap_or("").to_lowercase();
    let tokens: Vec<&str> = text::split(&tokens).filter(|token| SIGN_IN_AUTOCOMPLETE.contains(token)).collect();
    if tokens.contains(&"one-time-code") {
        return Some(CredentialField::OneTimeCode);
    }
    if is_password(control) || tokens.contains(&"current-password") {
        return Some(CredentialField::Password);
    }
    if tokens.contains(&"username") {
        return Some(CredentialField::Username);
    }
    tokens.contains(&"webauthn").then_some(CredentialField::Passkey)
}

/// Whether a field takes an account's credentials: the page's own evidence that it wants a sign-in.
pub fn asks_for_credentials(control: &Control) -> bool {
    credential_field(control).is_some()
}

/// What a wall asking for these credential fields wants: a password if any field is one, a one-time code if every
/// field is one, and otherwise a sign-in (an account name, a passkey, or only a control to sign in with).
pub fn wall_kind(fields: &[CredentialField]) -> WallKind {
    if fields.contains(&CredentialField::Password) {
        WallKind::Password
    } else if !fields.is_empty() && fields.iter().all(|field| *field == CredentialField::OneTimeCode) {
        WallKind::OneTimeCode
    } else {
        WallKind::SignIn
    }
}

/// An origin as its parts: scheme, host (IPv6 in brackets) and a port the scheme does not imply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl Origin {
    fn parse(url: &str) -> Option<Origin> {
        let parsed = Url::parse(text::strip(url)).ok()?;
        let host = parsed.host_str().filter(|h| !h.is_empty())?.to_lowercase();
        Some(Origin { scheme: parsed.scheme().to_string(), host, port: parsed.port() })
    }

    fn serialize(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// The web origin, with a port the scheme implies dropped rather than carried.
///
/// `https://shop.example.com` and `https://shop.example.com:443` are one origin, and a secret declared for one must be
/// typed on the other: a browser writes the port back either way after a navigation, and comparing the two as strings
/// dropped the secret and ended the run at needs_login. Credentials in the URL are dropped too, so `https://user@host`
/// cannot pass itself off as another origin. An address with no host is never one a secret is declared for: it comes
/// back as `scheme://`, or lowercased as written when it does not parse at all.
pub fn origin_of(url: &str) -> String {
    if let Some(origin) = Origin::parse(url) {
        return origin.serialize();
    }
    match Url::parse(text::strip(url)) {
        Ok(parsed) => format!("{}://", parsed.scheme()),
        Err(_) => text::strip(url).to_lowercase(),
    }
}

/// The host that a `*.` pattern covers: the host itself and every host under it.
const WILDCARD: &str = "*.";

/// Whether this secret may be typed on `origin`.
///
/// A declared origin is an exact one, or one whose host starts with `*.`, for a login that is the same login across a
/// site's hosts: `https://*.example.com` covers `www.example.com`, `accounts.example.com` and `example.com` itself.
/// The scheme and port must still match, and the wildcard only ever stands for whole labels, so it does not cover
/// `example.com.evil.test`, which merely ends with the same letters.
pub fn secret_allowed(secret: &SecretRef, origin: &str) -> bool {
    let Some(here) = Origin::parse(origin) else { return false };
    secret.origins.iter().any(|declared| {
        let Some(pattern) = Origin::parse(declared.trim_end_matches('/')) else { return false };
        // The scheme and port are never wildcarded: a secret for https is not for http, whatever the host.
        if (&pattern.scheme, pattern.port) != (&here.scheme, here.port) {
            return false;
        }
        if pattern.host == here.host {
            return true;
        }
        match pattern.host.strip_prefix(WILDCARD) {
            Some(suffix) if !suffix.is_empty() => here.host == suffix || here.host.ends_with(&format!(".{suffix}")),
            _ => false,
        }
    })
}

/// Only a secret declared for this origin resolves; anything else is treated as missing.
pub async fn resolve_secret(resolver: &dyn SecretResolver, name: &str, origin: &str) -> Option<String> {
    let secret = resolver.available().into_iter().find(|secret| secret.name == name)?;
    if !secret_allowed(&secret, origin) {
        return None;
    }
    resolver.resolve(name, origin).await
}

/// Secret values held in this process, each usable only where it was declared: a run's `SecretResolver`.
///
/// `ScopedSecrets::new(values, origin)` gives every value the same scope, which is what a run whose secrets all
/// belong to its start page needs. `ScopedSecrets::per_secret(...)` scopes each one separately, for a caller holding a
/// person's credentials against the sites each of them belongs to.
///
/// An origin may be a `*.` pattern either way, and the same rule decides here as everywhere: a resolver that answered
/// on a wider origin than it declared would put the gate in two places with two answers.
#[derive(Debug, Clone, Default)]
pub struct ScopedSecrets {
    secrets: IndexMap<String, (SecretValue, Vec<String>)>,
}

impl ScopedSecrets {
    pub fn new(values: impl IntoIterator<Item = (String, SecretValue)>, origin: &str) -> ScopedSecrets {
        let secrets = values.into_iter().map(|(name, value)| (name, (value, vec![origin.to_string()]))).collect();
        ScopedSecrets { secrets }
    }

    /// `{name: (value, origins)}`. A secret left with no origins is held by nobody and offered nowhere.
    pub fn per_secret(secrets: impl IntoIterator<Item = (String, (SecretValue, Vec<String>))>) -> ScopedSecrets {
        let secrets = secrets.into_iter().filter(|(_, (_, origins))| !origins.is_empty()).collect();
        ScopedSecrets { secrets }
    }
}

impl SecretResolver for ScopedSecrets {
    fn available(&self) -> Vec<SecretRef> {
        self.secrets
            .iter()
            .map(|(name, (_, origins))| SecretRef { name: name.clone(), origins: origins.clone() })
            .collect()
    }

    fn resolve<'a>(&'a self, name: &'a str, origin: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let (value, origins) = self.secrets.get(name)?;
            if !secret_allowed(&SecretRef { name: name.to_string(), origins: origins.clone() }, origin) {
                return None;
            }
            // A computed value is made only once the origin is allowed, and afresh each time it is typed.
            Some(value.get().await)
        })
    }
}

/// `json.dumps(value)[1:-1]`, with or without `ensure_ascii`.
fn json_escaped(value: &str, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_string(&mut out, value, ensure_ascii);
    out[1..out.len() - 1].to_string()
}

/// Replaces every resolved secret value with its name in anything written out of the process.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    values: IndexMap<String, String>,
    typed_on: HashMap<String, HashSet<String>>,
}

impl Redactor {
    pub fn new() -> Redactor {
        Redactor::default()
    }

    pub fn register(&mut self, name: &str, value: &str, origin: Option<&str>) {
        // Pages and logs carry a value encoded as often as raw: in a query string, or escaped inside JSON.
        let forms = [
            value.to_string(),
            text::quote(value),
            text::quote_plus(value),
            json_escaped(value, true),
            json_escaped(value, false),
        ];
        for form in forms {
            if form.is_empty() {
                continue;
            }
            if let Some(origin) = origin {
                self.typed_on.entry(form.clone()).or_default().insert(origin_of(origin));
            }
            self.values.insert(form, name.to_string());
        }
    }

    /// Replace each secret value with its name, except inside a host the secret was typed on (`spans`).
    pub fn redact(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end, name) in self.spans(text) {
            out.push_str(&text[at..start]);
            out.push_str(&format!("[secret:{name}]"));
            at = end;
        }
        out.push_str(&text[at..]);
        out
    }

    /// Blank secret values at equal length, so offsets into the text (capture blocks) stay valid. Length is Python's,
    /// in code points, so each character becomes one bullet whatever its width in bytes.
    pub fn mask(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end, _) in self.spans(text) {
            out.push_str(&text[at..start]);
            out.extend(std::iter::repeat_n('\u{2022}', text[start..end].chars().count()));
            at = end;
        }
        out.push_str(&text[at..]);
        out
    }

    pub fn reveals(&self, text: &str) -> bool {
        !self.spans(text).is_empty()
    }

    /// Where secret values sit in `text`, as byte ranges, merged where they overlap, each named for the longest value
    /// in it.
    ///
    /// A secret that is an ordinary word (`practice`) also names hosts: blanking it out of
    /// `https://practice.expandtesting.com/secure` left a row of bullets for the host in every address the model saw,
    /// and from there in every step, fact and citation link the run reported. An occurrence that sits wholly inside the
    /// host of an address on an origin the secret was typed on is left: the run read that address before the secret
    /// was ever typed there, and the site publishes its own host, so blanking it hides nothing and breaks the address.
    /// The value is still found everywhere else: in ordinary words, in the userinfo, path, query and fragment of every
    /// address, in any host it was not typed on, and where it runs across a host's edge.
    ///
    /// A value inside a `[secret:NAME]` marker this redactor wrote is left too, so redacting text twice changes nothing:
    /// a value `token` named `api_token` otherwise turned `[secret:api_token]` into `[secret:api_[secret:api_token]]`.
    fn spans(&self, text: &str) -> Vec<(usize, usize, String)> {
        let hosts = if self.typed_on.is_empty() { Vec::new() } else { hosts(text) };
        let markers = self.markers(text);
        let nowhere = HashSet::new();
        let mut found: Vec<(usize, usize, &str)> = Vec::new();
        for (value, name) in &self.values {
            let typed_on = self.typed_on.get(value).unwrap_or(&nowhere);
            let mut from = 0;
            while let Some(offset) = text[from..].find(value.as_str()) {
                let at = from + offset;
                let end = at + value.len();
                let published =
                    hosts.iter().any(|(lo, hi, origin)| *lo <= at && end <= *hi && typed_on.contains(origin));
                if !published && !markers.iter().any(|(lo, hi)| *lo <= at && end <= *hi) {
                    found.push((at, end, name));
                }
                // One character on, so overlapping occurrences are all found.
                from = at + text[at..].chars().next().map_or(1, char::len_utf8);
            }
        }
        // Longest first at each start, so a secret containing another secret is named for the whole.
        found.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
        let mut merged: Vec<(usize, usize, String)> = Vec::new();
        for (start, end, name) in found {
            match merged.last_mut() {
                Some(last) if start < last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end, name.to_string())),
            }
        }
        merged
    }

    /// Where `redact` has already replaced a value in `text`, as byte ranges.
    fn markers(&self, text: &str) -> Vec<(usize, usize)> {
        let names: HashSet<&String> = self.values.values().collect();
        names
            .into_iter()
            .flat_map(|name| {
                let marker = format!("[secret:{name}]");
                text.match_indices(&marker).map(|(at, _)| (at, at + marker.len())).collect::<Vec<_>>()
            })
            .collect()
    }

    /// Redact every string inside `value`, as a trace event is written out.
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_value(item)),
            Value::Object(fields) => fields.values_mut().for_each(|item| self.redact_value(item)),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

/// The host (and port) of every address in `text`, as a byte range with the origin it belongs to.
///
/// An address is `[A-Za-z][A-Za-z0-9+.-]*://` and an authority running to the first space, `/`, `?`, `#`, `\`, `<`,
/// `>`, a quote or a backtick.
fn hosts(text: &str) -> Vec<(usize, usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_alphabetic() {
            at += 1;
            continue;
        }
        let mut end = at + 1;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'+' | b'.' | b'-')) {
            end += 1;
        }
        if !text[end..].starts_with("://") {
            // No scheme starting inside this run ends anywhere else, so the search resumes after it.
            at = end;
            continue;
        }
        let netloc_start = end + 3;
        let length = text[netloc_start..]
            .find(|c: char| text::is_space(c) || matches!(c, '/' | '?' | '#' | '\\' | '<' | '>' | '"' | '\'' | '`'))
            .unwrap_or(text.len() - netloc_start);
        // A sentence's full stop or a closing bracket is not part of the host it follows.
        let netloc = text[netloc_start..netloc_start + length]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '}', '\'', '"']);
        if let Some(origin) = Origin::parse(&format!("{}://{netloc}", &text[at..end])) {
            let host_end = netloc_start + netloc.len();
            let host = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
            found.push((host_end - host.len(), host_end, origin.serialize()));
        }
        at = netloc_start + length;
    }
    found
}

#[cfg(test)]
mod tests {
    //! The behavior WHATWG changes, pinned.
    use super::*;
    use crate::testing::block_on;

    fn control(label: &str, role: &str, href: Option<&str>, input_type: Option<&str>) -> Control {
        let mut c = Control::new("c", role, label, [Operation::Click]);
        c.href = href.map(str::to_string);
        c.input_type = input_type.map(str::to_string);
        c
    }

    fn button(label: &str) -> Control {
        control(label, "button", None, None)
    }

    #[test]
    fn test_every_click_drag_and_enter_is_asked_about() {
        assert!(may_be_irreversible(Operation::Click));
        assert!(may_be_irreversible(Operation::Drag));
        // A message box with no form sends on Enter through the page's own script.
        assert!(may_be_irreversible(Operation::Enter));
        assert!(!may_be_irreversible(Operation::Fill));
        assert!(!may_be_irreversible(Operation::Navigate));
    }

    #[test]
    fn test_a_secret_is_caught_in_every_encoding_a_page_or_log_carries_it_in() {
        let mut redactor = Redactor::new();
        redactor.register("password", "p@ss w\"ord", None);
        for text in ["p@ss w\"ord", "p%40ss%20w%22ord", "p%40ss+w%22ord", "{\"v\": \"p@ss w\\\"ord\"}"] {
            assert!(redactor.reveals(text), "{text}");
            assert!(!redactor.redact(text).replace("[secret:password]", "").contains("ss"), "{text}");
        }
    }

    #[test]
    fn test_a_secret_is_redacted_from_an_address_everywhere_but_the_host_it_was_typed_on() {
        let mut redactor = Redactor::new();
        redactor.register("username", "practice", Some("https://practice.expandtesting.com/login"));
        for (url, redacted) in [
            ("https://practice.expandtesting.com/secure", "https://practice.expandtesting.com/secure"),
            ("https://x.com/login?user=practice", "https://x.com/login?user=[secret:username]"),
            (
                "https://practice:pw@x.com:8443/practice#practice",
                "https://[secret:username]:pw@x.com:8443/[secret:username]#[secret:username]",
            ),
            ("https://practice.expandtesting.com:443/", "https://practice.expandtesting.com:443/"),
            ("https://practice.attacker.test/", "https://[secret:username].attacker.test/"),
            ("https://[secret", "https://[secret"),
        ] {
            assert_eq!(redactor.redact(url), redacted, "{url}");
        }
        assert_eq!(redactor.redact("sign in as practice"), "sign in as [secret:username]");
    }

    #[test]
    fn test_redacting_text_twice_leaves_the_first_pass_markers_alone() {
        let mut redactor = Redactor::new();
        redactor.register("api_token", "token", None);
        let once = redactor.redact("sent token, then token");
        assert_eq!(once, "sent [secret:api_token], then [secret:api_token]");
        assert_eq!(redactor.redact(&once), once);
        assert!(!redactor.reveals(&once));
    }

    #[test]
    fn test_masking_keeps_the_host_a_secret_was_typed_on_and_blanks_every_other_appearance() {
        let mut redactor = Redactor::new();
        redactor.register("username", "practice", Some("https://practice.expandtesting.com/login"));
        redactor.register("password", "hunter2", Some("https://practice.expandtesting.com/login"));
        let b = |n: usize| "\u{2022}".repeat(n);
        for (text, masked) in [
            // The site's own host, wherever the address sits, keeps its letters; the rest of the address does not.
            (
                "https://practice.expandtesting.com/secure".to_string(),
                "https://practice.expandtesting.com/secure".to_string(),
            ),
            ("see https://practice.expandtesting.com.".into(), "see https://practice.expandtesting.com.".into()),
            (
                "https://practice.expandtesting.com/?u=practice".into(),
                format!("https://practice.expandtesting.com/?u={}", b(8)),
            ),
            (
                "https://practice@practice.expandtesting.com/".into(),
                format!("https://{}@practice.expandtesting.com/", b(8)),
            ),
            // Anywhere else the value is blanked, the ordinary word and any host it was not typed on included.
            ("practice makes perfect".into(), format!("{} makes perfect", b(8))),
            ("https://practice.attacker.test/".into(), format!("https://{}.attacker.test/", b(8))),
            ("http://practice.expandtesting.com/".into(), format!("http://{}.expandtesting.com/", b(8))),
            // A password in the host of any other origin is blanked, a sibling host of the same site included.
            ("https://hunter2.expandtesting.com/".into(), format!("https://{}.expandtesting.com/", b(7))),
            ("https://x.test/#hunter2 hunter2".into(), format!("https://x.test/#{} {}", b(7), b(7))),
            ("hunter2practice".into(), b(15)),
        ] {
            assert_eq!(redactor.mask(&text), masked, "{text}");
            assert_eq!(redactor.mask(&text).chars().count(), text.chars().count());
            assert_eq!(redactor.reveals(&text), masked != text, "{text}");
        }
    }

    #[test]
    fn test_a_secret_running_from_the_host_into_the_path_is_still_redacted() {
        let mut redactor = Redactor::new();
        redactor.register("token", "example.test/abc", None);
        assert_eq!(redactor.redact("https://example.test/abc"), "https://[secret:token]");
    }

    #[test]
    fn test_a_port_the_scheme_implies_is_not_part_of_the_origin() {
        for (url, origin) in [
            ("https://shop.example.test:443/cart", "https://shop.example.test"),
            ("http://shop.example.test:80/", "http://shop.example.test"),
            ("https://SHOP.example.test/", "https://shop.example.test"),
            ("https://shop.example.test:8443/", "https://shop.example.test:8443"),
            ("http://shop.example.test:443/", "http://shop.example.test:443"),
            ("https://user:pw@shop.example.test/", "https://shop.example.test"),
        ] {
            assert_eq!(origin_of(url), origin, "{url}");
        }
    }

    fn secret(origins: &[&str]) -> SecretRef {
        SecretRef { name: "password".into(), origins: origins.iter().map(|o| o.to_string()).collect() }
    }

    #[test]
    fn test_a_secret_declared_without_the_port_is_typed_on_the_url_that_carries_it() {
        let declared = secret(&["https://shop.example.test"]);
        assert!(secret_allowed(&declared, &origin_of("https://shop.example.test:443/login")));
        assert!(!secret_allowed(&declared, &origin_of("https://shop.example.test:8443/login")));
    }

    #[test]
    fn test_a_secret_declared_for_a_site_covers_its_hosts_and_nothing_that_merely_looks_like_them() {
        let declared = secret(&["https://*.example.test"]);
        for (origin, allowed) in [
            ("https://www.example.test", true),
            ("https://accounts.eu.example.test", true),
            // One login across a site's hosts includes the bare domain: nobody writing the pattern means to exclude it.
            ("https://example.test", true),
            // The wildcard stands for whole labels, so a domain that merely ends with the same letters is another site.
            ("https://example.test.evil.test", false),
            ("https://notexample.test", false),
            // Neither the scheme nor the port is ever wildcarded.
            ("http://www.example.test", false),
            ("https://www.example.test:8443", false),
        ] {
            assert_eq!(secret_allowed(&declared, &origin_of(origin)), allowed, "{origin}");
        }
    }

    #[test]
    fn test_a_wildcard_that_covers_nothing_is_not_a_wildcard_that_covers_everything() {
        // A pattern with no domain after it would otherwise match whatever the run opened.
        for pattern in ["https://*.", "https://*"] {
            assert!(!secret_allowed(&secret(&[pattern]), &origin_of("https://evil.test")));
        }
    }

    #[test]
    fn test_an_ipv6_origin_survives_being_parsed_again() {
        for (url, origin) in [
            ("https://[::1]/login", "https://[::1]"),
            ("https://[::1]:8443/", "https://[::1]:8443"),
            ("http://[2001:db8::1]:80/", "http://[2001:db8::1]"),
        ] {
            // Every check here reparses what this returns.
            assert_eq!(origin_of(url), origin);
            let declared = secret(&[origin]);
            assert!(secret_allowed(&declared, &origin_of(url)));
            assert!(!secret_allowed(&declared, &origin_of("https://[::2]/login")));
        }
    }

    #[test]
    fn test_each_secret_keeps_the_scope_it_was_declared_with() {
        let held = ScopedSecrets::per_secret([
            ("shop".to_string(), (SecretValue::from("shop-password"), vec!["https://*.example.test".to_string()])),
            ("bank".to_string(), (SecretValue::from("bank-password"), vec!["https://secure.bank.test".to_string()])),
            ("orphan".to_string(), (SecretValue::from("nowhere"), vec![])),
        ]);
        let names: Vec<String> = held.available().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["shop", "bank"]);
        let resolve = |name: &str, url: &str| block_on(held.resolve(name, &origin_of(url)));
        // The wildcard is not narrowed to whichever host was opened first.
        assert_eq!(resolve("shop", "https://accounts.example.test").as_deref(), Some("shop-password"));
        assert_eq!(resolve("shop", "https://www.example.test").as_deref(), Some("shop-password"));
        // One secret's scope is not another's.
        assert_eq!(resolve("bank", "https://www.example.test"), None);
        assert_eq!(resolve("bank", "https://secure.bank.test").as_deref(), Some("bank-password"));
        // A secret declared for nowhere is offered nowhere, rather than everywhere.
        assert_eq!(resolve("orphan", "https://www.example.test"), None);
        assert_eq!(block_on(resolve_secret(&held, "bank", "https://www.example.test")), None);
    }

    #[test]
    fn test_starts_download_reads_the_page_evidence() {
        for (href, download, expected) in [
            (Some("/files/addon-2.1.0.zip?sig=1"), None, true),
            (Some("cdn.example.com/releases/Setup.EXE"), None, true),
            (Some("/export"), Some(true), true),
            (Some("/releases/2.1.0"), None, false),
            (Some("/guide.pdf"), None, false),
            (Some("https://downloads.zip"), None, false),
            (None, None, false),
        ] {
            let mut link = Control::new("a", "link", "Get it", []);
            link.href = href.map(str::to_string);
            link.download = download;
            assert_eq!(starts_download(&link), expected, "{href:?}");
        }
    }

    #[test]
    fn test_focusing_a_field_does_not_claim_to_submit_its_form() {
        let mut field = Control::new("b0", "textbox", "Name", [Operation::Click, Operation::Fill, Operation::Enter]);
        field.submit_semantics = Some("POST /delete-all".into());
        assert!(
            !irreversible_question("Inspect", Operation::Click, &field, None)
                .instructions
                .contains("submits this form")
        );
        assert!(
            irreversible_question("Inspect", Operation::Enter, &field, None).instructions.contains("submits this form")
        );
    }

    #[test]
    fn the_irreversible_question_quotes_labels_as_python_repr_does() {
        let mut target = button("Yes, I'm sure");
        target.context = Some("Delete \"all\"".into());
        let question = irreversible_question("Tidy up", Operation::Click, &target, None);
        assert!(question.instructions.starts_with(&format!(
            "{UNTRUSTED}\nThe agent is about to click the element labelled \"Yes, I'm sure\" while doing this task: Tidy up\nIt sits under 'Delete \"all\"' on the page.\n"
        )));
    }

    #[test]
    fn test_the_requested_question_tells_two_submit_buttons_apart() {
        let mut target = button("Submit");
        target.context = Some("Delete account".into());
        target.submit_semantics = Some("POST /account/delete".into());
        target.form_id = Some("f".into());
        let mut field = Control::new("m", "textbox", "Message", [Operation::Fill]);
        field.form_id = Some("f".into());
        field.value = Some("Please close my account".into());
        let mut secret = password("p", Some("f"), None);
        secret.value = Some("••••".into());
        let action = control_action(Operation::Click, &target, &[target.clone(), field, secret]);
        let question = requested_question("Update my newsletter preferences", &action);
        assert!(question.instructions.contains("It sits under 'Delete account' on the page.\n"));
        assert!(question.instructions.contains("The action submits this form: POST /account/delete\n"));
        assert!(question.instructions.contains("In the form, 'Message' holds 'Please close my account'.\n"));
    }

    fn password(id: &str, form: Option<&str>, autocomplete: Option<&str>) -> Control {
        let mut field = Control::new(id, "textbox", "Password", [Operation::Fill]);
        field.sensitive = true;
        field.input_type = Some("password".into());
        field.form_id = form.map(str::to_string);
        field.autocomplete = autocomplete.map(str::to_string);
        field
    }

    #[test]
    fn a_password_change_is_recognized_from_the_forms_structure_not_its_wording() {
        let mut save = button("Save");
        save.form_id = Some("f".into());
        let (current, new) = (password("p0", Some("f"), None), password("p1", Some("f"), None));
        assert!(changes_credentials(&[current.clone(), new.clone()], &save, Operation::Click));
        assert!(changes_credentials(&[current.clone(), new.clone()], &current, Operation::Enter));
        // A sign-in form has one password field, whatever its button says.
        let mut reset = button("Reset your password");
        reset.form_id = Some("f".into());
        assert!(!changes_credentials(std::slice::from_ref(&current), &reset, Operation::Click));
        // A sign-in page marks its one password field new-password to keep browsers from filling it.
        assert!(!changes_credentials(&[password("p", Some("f"), Some("new-password"))], &save, Operation::Click));
        // Another form's fields, a link and a field's focus are not this form's submission.
        let mut other = save.clone();
        other.form_id = Some("g".into());
        assert!(!changes_credentials(&[current.clone(), new.clone()], &other, Operation::Click));
        let mut link = control("Change password", "link", Some("/settings"), None);
        link.form_id = Some("f".into());
        assert!(!changes_credentials(&[current.clone(), new.clone()], &link, Operation::Click));
        assert!(!changes_credentials(&[current.clone(), new.clone()], &current, Operation::Click));
        assert!(asks_for_credentials(&new));
        let mut otp = Control::new("o", "textbox", "Code", [Operation::Fill]);
        otp.autocomplete = Some("One-Time-Code".into());
        assert!(asks_for_credentials(&otp));
    }

    #[test]
    fn a_replacement_password_field_is_told_by_structure_and_an_unclear_one_is_not_guessed() {
        let (current, new, again) =
            (password("p0", Some("f"), None), password("p1", Some("f"), None), password("p2", Some("f"), None));
        let form = [current.clone(), new.clone(), again.clone()];
        assert_eq!(sets_new_password(&form, &current), Some(false));
        assert_eq!(sets_new_password(&form, &new), Some(true));
        assert_eq!(sets_new_password(&form, &again), Some(true));
        let sign_in = password("p", Some("f"), None);
        assert_eq!(sets_new_password(std::slice::from_ref(&sign_in), &sign_in), Some(false));
        let marked = password("p", Some("f"), Some("new-password"));
        assert_eq!(sets_new_password(std::slice::from_ref(&marked), &marked), Some(false));
        let (first, second) = (password("n", Some("f"), Some("new-password")), password("c", Some("f"), None));
        assert_eq!(sets_new_password(&[first.clone(), second.clone()], &first), Some(true));
        let loose = [password("a", None, None), password("b", None, None)];
        assert_eq!(sets_new_password(&loose, &loose[1]), None);
        assert_eq!(sets_new_password(&loose[..1], &loose[0]), Some(false));
        assert_eq!(sets_new_password(&loose, &password("c", None, Some("current-password"))), Some(false));
    }

    /// A username that took a stored secret is masked like a password, and is still not one.
    #[test]
    fn a_sign_in_form_whose_username_holds_a_secret_takes_the_current_password() {
        let mut username = Control::new("u", "textbox", "Username", [Operation::Fill]);
        (username.sensitive, username.input_type, username.form_id) = (true, Some("text".into()), Some("f".into()));
        let password = password("p", Some("f"), None);
        let mut login = button("Login");
        login.form_id = Some("f".into());
        let form = [username.clone(), password.clone(), login.clone()];
        assert_eq!(sets_new_password(&form, &password), Some(false));
        assert!(!changes_credentials(&form, &login, Operation::Click));
        assert!(!changes_credentials(&form, &password, Operation::Enter));
        assert_ne!(credential_field(&username), Some(CredentialField::Password));
    }

    /// A sign-in form whose one password field is marked new-password, as pages do to keep browsers from filling it.
    #[test]
    fn a_sign_in_form_marking_its_one_password_new_takes_the_current_password() {
        let mut username = Control::new("u", "textbox", "Username", [Operation::Fill]);
        (username.form_id, username.autocomplete) = (Some("f".into()), Some("off".into()));
        let password = password("p", Some("f"), Some("new-password"));
        let mut login = button("Login");
        login.form_id = Some("f".into());
        let form = [username, password.clone(), login.clone()];
        assert_eq!(sets_new_password(&form, &password), Some(false));
        assert!(!changes_credentials(&form, &login, Operation::Click));
    }

    #[test]
    fn a_wall_is_named_for_the_credential_its_fields_ask_for() {
        let field = |sensitive: bool, input_type: Option<&str>, autocomplete: Option<&str>| {
            let mut control = Control::new("f", "textbox", "Field", [Operation::Fill]);
            control.sensitive = sensitive;
            control.input_type = input_type.map(str::to_string);
            control.autocomplete = autocomplete.map(str::to_string);
            credential_field(&control)
        };
        let code = field(true, Some("password"), Some("One-Time-Code"));
        let password = field(false, Some("password"), None);
        let username = field(false, Some("email"), Some("username"));
        assert_eq!(code, Some(CredentialField::OneTimeCode));
        assert_eq!(password, Some(CredentialField::Password));
        assert_eq!(field(false, None, Some("current-password")), Some(CredentialField::Password));
        assert_eq!(username, Some(CredentialField::Username));
        assert_eq!(field(false, Some("text"), Some("name")), None);
        for (fields, kind) in [
            (vec![code, code], WallKind::OneTimeCode),
            (vec![username, password], WallKind::Password),
            (vec![code, password], WallKind::Password),
            (vec![username], WallKind::SignIn),
            (vec![username, code], WallKind::SignIn),
            (vec![], WallKind::SignIn),
        ] {
            let fields: Vec<CredentialField> = fields.into_iter().flatten().collect();
            assert_eq!(wall_kind(&fields), kind, "{fields:?}");
        }
    }

    /// Where WHATWG and `urlsplit` disagree, the origin is the one Chrome loads.
    #[test]
    fn an_origin_is_the_one_chrome_loads() {
        // An internationalized host is written in its ASCII form, as Chrome reports it; urlsplit kept the letters.
        assert_eq!(origin_of("https://b\u{fc}cher.example/"), "https://xn--bcher-kva.example");
        // A port every special scheme implies is dropped, not only http's and https's.
        assert_eq!(origin_of("ftp://files.example:21/"), "ftp://files.example");
        // A backslash ends the authority in a special URL, so the host is the one before it.
        assert_eq!(origin_of("https://a.example\\@evil.example/"), "https://a.example");
        // An address with no host comes back as its scheme alone, never as a host to type on.
        assert_eq!(origin_of("about:blank"), "about://");
        assert!(!secret_allowed(&secret(&["https://a.example"]), "about:blank"));
    }
}
