//! The rendered text of one served document as the blocks a reader cites, without running it.
//!
//! html5ever's tokenizer reads the tags, the raw-text elements and the character references as a browser's does; the
//! reader turns them into the blocks `capture.js` makes of the same page.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use encoding_rs::{Encoding, UTF_8};
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{BlockKind, Control};
use fastbrowse_core::py::text;
use html5ever::Attribute;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};

/// Scripts, and the elements whose only content is a script or a picture, state nothing a reader can quote.
/// `embed` is void, so it has no content to skip and no end tag that would end the skip.
const SKIPPED: [&str; 10] =
    ["script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "audio", "video"];
/// Whether two runs of text belong to one block: a block-level tag ends one, an inline tag does not.
const BLOCKS: [&str; 27] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "ul",
];
fn heading_level(tag: &str) -> Option<usize> {
    match tag.as_bytes() {
        [b'h', level @ b'1'..=b'6'] => Some(usize::from(level - b'0')),
        _ => None,
    }
}

/// Whether an element's own `style` declares `display: none` or `visibility: hidden`, which a browser never renders.
fn hides(style: &str) -> bool {
    let lower = style.to_ascii_lowercase();
    let declares = |property: &str, value: &str| {
        lower.match_indices(property).any(|(at, _)| {
            let rest = lower[at + property.len()..].trim_start_matches(text::is_space);
            rest.strip_prefix(':').is_some_and(|rest| rest.trim_start_matches(text::is_space).starts_with(value))
        })
    };
    declares("display", "none") || declares("visibility", "hidden")
}

/// Text with the whitespace a browser would collapse, as `capture.js` writes it.
///
/// Quotes are validated against what a reader sees, so the same page has to read the same way whether it was captured
/// from the DOM or fetched: a stray newline from markup would put a value on its own line and break a quote taken from
/// the other reader.
pub fn clean(raw: &str) -> String {
    // `[ \t\r\f\v]+` becomes one space, then ` *\n *` one newline.
    let mut spaced = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            ' ' | '\t' | '\r' | '\u{c}' | '\u{b}' => {
                if !spaced.ends_with(' ') {
                    spaced.push(' ');
                }
            }
            '\n' => {
                while spaced.ends_with(' ') {
                    spaced.pop();
                }
                spaced.push('\n');
            }
            c if c != ' ' && spaced.ends_with("\n ") => {
                spaced.pop();
                spaced.push(c);
            }
            c => spaced.push(c),
        }
    }
    if spaced.ends_with("\n ") {
        spaced.pop();
    }
    // `\n{3,}` becomes two.
    let mut collapsed = String::with_capacity(spaced.len());
    let mut newlines = 0;
    for c in spaced.chars() {
        newlines = if c == '\n' { newlines + 1 } else { 0 };
        if newlines <= 2 {
            collapsed.push(c);
        }
    }
    text::strip(&collapsed).to_string()
}

/// What reads the tokens of a document. Each tag token is numbered in order, so the [`Tree`] read first and the
/// [`Reader`] read after it name the same element by its start tag.
trait Tags {
    fn start(&mut self, tag: &Tag, at: usize);
    fn end(&mut self, name: &str, at: usize);
    fn data(&mut self, data: &str);
}

/// The tokenizer's tokens, handed to a [`Tags`]. A start tag also sets the tokenizer's state, as the tree builder
/// would, so a script's or a title's text is read as text up to its own end tag.
struct Sink<T>(RefCell<(T, usize)>);

impl<T> Sink<T> {
    fn new(tags: T) -> Sink<T> {
        Sink(RefCell::new((tags, 0)))
    }

    fn into_inner(self) -> T {
        self.0.into_inner().0
    }
}

impl<T: Tags> TokenSink for Sink<T> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut sink = self.0.borrow_mut();
        let (tags, count) = &mut *sink;
        match token {
            Token::TagToken(tag) => {
                let at = *count;
                *count += 1;
                if tag.kind == TagKind::EndTag {
                    tags.end(&tag.name, at);
                    return TokenSinkResult::Continue;
                }
                tags.start(&tag, at);
                return match &*tag.name {
                    "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
                    "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                        TokenSinkResult::RawData(RawKind::Rawtext)
                    }
                    "script" => TokenSinkResult::RawData(RawKind::ScriptData),
                    "plaintext" => TokenSinkResult::Plaintext,
                    _ => TokenSinkResult::Continue,
                };
            }
            Token::CharacterTokens(data) => tags.data(&data),
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// Every token of `html`, handed to `sink` in order.
fn tokenize<S: TokenSink>(html: &str, sink: S) -> S {
    let tokenizer = Tokenizer::new(sink, TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink
}

/// Elements whose default style is not inline, as `capture.js` tells a block by its computed display.
fn displays_block(tag: &str) -> bool {
    BLOCKS.contains(&tag)
        || heading_level(tag).is_some()
        || matches!(tag, "br" | "caption" | "center" | "details" | "dialog" | "hgroup" | "legend" | "menu" | "summary")
        || matches!(tag, "td" | "th" | "tr")
}

/// Elements with no content and no end tag.
const VOID: [&str; 13] =
    ["area", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
/// Elements that end the search for an open element an end tag closes, as the tree builder's scopes do.
const SCOPES: [&str; 6] = ["caption", "html", "table", "td", "template", "th"];
/// The elements `capture.js` lets a record's facts stand on without naming them.
const LEAVES_OF_THEIR_OWN: [&str; 7] = ["a", "button", "img", "input", "select", "textarea", "svg"];

/// One element of the served markup, in the tree a browser builds of it.
struct Element {
    tag: String,
    parent: usize,
    children: Vec<usize>,
    /// The class tokens, in order and without repeats.
    classes: Vec<String>,
    href: Option<String>,
    id: Option<String>,
    /// Whether its `role` names it a note reference (`doc-noteref`).
    noteref: bool,
    /// Whether text other than white space sits directly in it, not in a child.
    own_text: bool,
    /// The `aria-label`, else the `title`, which names a fact the element only draws.
    label: Option<String>,
    navigation: bool,
    /// Never rendered: hidden itself, skipped, or inside such an element.
    hidden: bool,
    /// Inside `svg` or `math`, where a self-closing tag ends its element.
    foreign: bool,
    /// Whether it shows any text.
    text: bool,
    /// The tag tokens that opened and closed it.
    start: usize,
    close: usize,
    /// Its last descendant: elements are numbered in document order, so its descendants are the ones up to this.
    last: usize,
}

/// The served markup's elements, built by a first read so the reader knows a card's siblings and descendants before
/// it reads the card's text. The rules are the tree builder's for the cases cards use: void elements, and the end tags
/// a `p`, a list item, an option or a table part implies.
struct Tree {
    url: String,
    /// The document itself is element 0.
    elements: Vec<Element>,
    open: Vec<usize>,
}

impl Tree {
    fn new(url: &str) -> Tree {
        let document = Element {
            tag: String::new(),
            parent: 0,
            children: Vec::new(),
            classes: Vec::new(),
            href: None,
            id: None,
            noteref: false,
            own_text: false,
            label: None,
            navigation: false,
            hidden: false,
            foreign: false,
            text: false,
            start: usize::MAX,
            close: usize::MAX,
            last: 0,
        };
        Tree { url: url.to_string(), elements: vec![document], open: vec![0] }
    }

    fn top(&self) -> &Element {
        &self.elements[*self.open.last().unwrap_or(&0)]
    }

    /// Closes the innermost open `targets` element and every element inside it, unless one of `stops` is nearer.
    fn close_through(&mut self, targets: &[&str], stops: impl Fn(&str) -> bool, at: usize) {
        let Some(found) = self.open.iter().skip(1).rposition(|&i| {
            let tag = self.elements[i].tag.as_str();
            targets.contains(&tag) || stops(tag)
        }) else {
            return;
        };
        let found = found + 1;
        if targets.contains(&self.elements[self.open[found]].tag.as_str()) {
            self.close_from(found, at);
        }
    }

    /// Closes the open elements from `found` in, at the tag token `at`; nothing after this is inside them.
    fn close_from(&mut self, found: usize, at: usize) {
        let last = self.elements.len() - 1;
        for i in self.open.drain(found..) {
            self.elements[i].close = at;
            self.elements[i].last = last;
        }
    }

    /// Closes what is open inside the innermost `within` element, when a table part starts.
    fn close_back_to(&mut self, within: &[&str], at: usize) {
        let Some(found) = self.open.iter().rposition(|&i| within.contains(&self.elements[i].tag.as_str())) else {
            return;
        };
        self.close_from(found + 1, at);
    }

    /// The end tags a start tag implies.
    fn imply_ends(&mut self, tag: &str, at: usize) {
        let closes_p = displays_block(tag) && !matches!(tag, "br" | "td" | "th" | "tr" | "caption");
        if closes_p {
            self.close_through(&["p"], |open| SCOPES.contains(&open) || open == "button", at);
        }
        // A list item, or a term or its description, ends the one open in the same list.
        let list_stop = |open: &str| displays_block(open) && !matches!(open, "address" | "div" | "p");
        match tag {
            "li" => self.close_through(&["li"], list_stop, at),
            "dt" | "dd" => self.close_through(&["dt", "dd"], list_stop, at),
            "option" | "optgroup" if self.top().tag == "option" => self.close_through(&["option"], |_| true, at),
            "tr" => self.close_back_to(&["table", "tbody", "thead", "tfoot"], at),
            "td" | "th" => self.close_back_to(&["tr", "table"], at),
            "tbody" | "thead" | "tfoot" => self.close_back_to(&["table"], at),
            "a" => self.close_through(&["a"], |open| SCOPES.contains(&open), at),
            _ if heading_level(tag).is_some() && heading_level(&self.top().tag).is_some() => {
                let open = self.top().tag.clone();
                self.close_through(&[open.as_str()], |_| true, at);
            }
            _ => {}
        }
        if tag == "optgroup" && self.top().tag == "optgroup" {
            self.close_through(&["optgroup"], |_| true, at);
        }
    }

    /// The tag tokens opening the footnote references, which `capture.js` leaves out of the text they mark: a link
    /// the markup names a note reference, or an in-page link whose target links back to exactly that link, or to a
    /// wrapper showing nothing but it. A table of contents or a "back to top" link is never answered by a link to
    /// itself.
    fn note_references(&mut self) -> HashSet<usize> {
        self.close_from(1, usize::MAX);
        let elements = &self.elements;
        let mut ids: HashMap<&str, usize> = HashMap::new();
        for (i, element) in elements.iter().enumerate().skip(1) {
            if let Some(id) = element.id.as_deref() {
                // The first element with an id is the one a fragment reaches.
                ids.entry(id).or_insert(i);
            }
        }
        let target = |href: Option<&str>| {
            let fragment = href?.strip_prefix('#').filter(|fragment| !fragment.is_empty())?;
            let decoded = percent_encoding::percent_decode_str(fragment).decode_utf8().ok();
            decoded.and_then(|id| ids.get(&*id)).or_else(|| ids.get(fragment)).copied()
        };
        let within = |i: usize, outer: usize| outer <= i && i <= elements[outer].last;
        // Whether `wrapper` shows the link `link` and no text of its own beside it.
        let shows_only = |wrapper: usize, link: usize| {
            within(link, wrapper)
                && (wrapper..=elements[wrapper].last)
                    .all(|i| within(i, link) || !elements[i].own_text || elements[i].hidden)
        };
        let mut found = HashSet::new();
        for (i, element) in elements.iter().enumerate().skip(1) {
            if element.hidden {
                continue;
            }
            let referenced = element.noteref
                || (element.tag == "a"
                    && target(element.href.as_deref()).is_some_and(|note| {
                        !within(i, note)
                            && (note + 1..=elements[note].last).any(|back| {
                                elements[back].tag == "a"
                                    && target(elements[back].href.as_deref())
                                        .is_some_and(|wrapper| shows_only(wrapper, i))
                            })
                    }));
            if referenced {
                found.insert(element.start);
            }
        }
        found
    }

    /// Marks an element and the elements around it as showing text.
    fn shows_text(&mut self, mut i: usize) {
        while i != 0 && !self.elements[i].text {
            self.elements[i].text = true;
            i = self.elements[i].parent;
        }
    }
}

impl Tags for Tree {
    fn start(&mut self, tag: &Tag, at: usize) {
        let name = &*tag.name;
        let get = |name: &str| tag.attrs.iter().find(|a| &*a.name.local == name).map(|a| a.value.to_string());
        if !self.top().foreign {
            self.imply_ends(name, at);
        }
        let parent = *self.open.last().unwrap_or(&0);
        let (hidden, foreign) = (self.elements[parent].hidden, self.elements[parent].foreign);
        let mut classes: Vec<String> = Vec::new();
        for class in get("class").unwrap_or_default().split_ascii_whitespace() {
            if !classes.iter().any(|seen| seen == class) {
                classes.push(class.to_string());
            }
        }
        let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
        let i = self.elements.len();
        let foreign = foreign || matches!(name, "svg" | "math");
        self.elements.push(Element {
            tag: name.to_string(),
            parent,
            children: Vec::new(),
            classes,
            href: get("href"),
            id: get("id"),
            noteref: get("role").is_some_and(|role| role.split_ascii_whitespace().any(|role| role == "doc-noteref")),
            own_text: false,
            label: nonempty(get("aria-label")).or_else(|| nonempty(get("title"))),
            navigation: get("role").is_some_and(|role| role == "navigation"),
            hidden: hidden
                || SKIPPED.contains(&name)
                || get("hidden").is_some()
                || hides(&get("style").unwrap_or_default()),
            foreign,
            text: false,
            start: at,
            close: usize::MAX,
            last: i,
        });
        self.elements[parent].children.push(i);
        // A dated element shows its date, as the reader writes it.
        let dated =
            (name == "time" || name.contains('-')) && get("datetime").is_some_and(|d| !text::strip(&d).is_empty());
        if dated && !self.elements[i].hidden {
            self.shows_text(i);
        }
        if VOID.contains(&name) || (foreign && tag.self_closing) {
            self.elements[i].close = at;
        } else {
            self.open.push(i);
        }
    }

    fn end(&mut self, name: &str, at: usize) {
        // The document's own elements stay open to its end, as a browser keeps them.
        if !matches!(name, "html" | "body" | "head") {
            self.close_through(&[name], |open| SCOPES.contains(&open), at);
        }
    }

    fn data(&mut self, data: &str) {
        let top = *self.open.last().unwrap_or(&0);
        if !self.elements[top].hidden && !text::strip(data).is_empty() {
            self.elements[top].own_text = true;
            self.shows_text(top);
        }
    }
}

/// A repeated card, read as one block: what `capture.js` calls a record.
struct Record {
    heading_level: Option<usize>,
    /// The tag token that closes the card.
    close: usize,
    kind: BlockKind,
    /// The card's one link, when it has only one.
    href: Option<String>,
    /// What the card only draws, each fact named as the page names it.
    facts: String,
}

/// What an element's descendants hold, for the rules that look inside a card.
#[derive(Clone, Copy, Default)]
struct Holds {
    framed: bool,
    block: bool,
    /// A record, or a list item read as one block.
    item: bool,
    links: usize,
    first_link: Option<usize>,
    leaf_of_its_own: bool,
    heading_level: Option<usize>,
}

impl Holds {
    fn add(&mut self, other: Holds) {
        self.framed |= other.framed;
        self.block |= other.block;
        self.item |= other.item;
        self.links += other.links;
        self.first_link = self.first_link.or(other.first_link);
        self.leaf_of_its_own |= other.leaf_of_its_own;
        self.heading_level = self.heading_level.into_iter().chain(other.heading_level).min();
    }
}

impl Tree {
    fn link(&self, i: usize) -> bool {
        self.elements[i].tag == "a" && self.elements[i].href.is_some()
    }

    /// A list item with no block of its own inside, which `capture.js` reads as one block.
    fn leaf(&self, i: usize) -> bool {
        !self.elements[i].children.iter().any(|&c| !self.elements[c].hidden && displays_block(&self.elements[c].tag))
    }

    /// Whether an element is one of a run of cards, given the siblings of its tag and class set and the headings each
    /// element holds, itself included.
    fn repeated(&self, i: usize, siblings: &[usize], headings: &[usize]) -> bool {
        let element = &self.elements[i];
        // A classless div repeated three times is usually page layout; a record is a styled unit or a semantic item.
        if element.classes.is_empty() && !matches!(element.tag.as_str(), "article" | "li") {
            return false;
        }
        if siblings.len() >= 3 {
            return true;
        }
        if siblings.len() != 2 {
            return false;
        }
        // Linked cards can sit inside an unlabelled list wrapper, so distinct destinations establish the pair.
        if element.tag == "a" && siblings.iter().all(|&s| self.elements[s].href.is_some()) {
            let href = |s: usize| join(&self.url, self.elements[s].href.as_deref().unwrap_or_default());
            return href(siblings[0]) != href(siblings[1]);
        }
        // A styled pair can be a two-column layout, so require a heading outside both items to name their group.
        let parent = element.parent;
        let around = headings[parent] - usize::from(heading_level(&self.elements[parent].tag).is_some());
        parent != 0 && around > siblings.iter().map(|&s| headings[s]).sum::<usize>()
    }

    /// The facts a card states only by drawing them, as a row of identical icons a class colours: each element with
    /// no text that its label or class names, written as the page names it and said to show no text, as `capture.js`
    /// writes it (#992).
    fn drawn_facts(&self, i: usize, holds: &[Holds]) -> String {
        let mut facts = String::new();
        let descendants = i + 1..=self.elements[i].last;
        for (element, inside) in self.elements[descendants.clone()].iter().zip(&holds[descendants]) {
            let own_leaf = LEAVES_OF_THEIR_OWN.contains(&element.tag.as_str());
            if element.children.is_empty() || element.hidden || element.text || own_leaf || inside.leaf_of_its_own {
                continue;
            }
            if element.parent != i && !self.elements[element.parent].text {
                continue;
            }
            let name = clean(&element.label.clone().unwrap_or_else(|| element.classes.join(" ")));
            if !name.is_empty() {
                facts.push_str(&format!(" [{name}: no text]"));
            }
        }
        facts
    }

    /// The cards of the document, by the tag token that opens each, under the rules `capture.js` applies.
    fn records(mut self) -> HashMap<usize, Record> {
        self.close_from(1, usize::MAX);
        let elements = &self.elements;
        let count = elements.len();
        // Siblings of the same tag and class set, by element.
        let mut runs: Vec<usize> = vec![0; count];
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for element in elements {
            let mut keyed: HashMap<(&str, Vec<&str>), usize> = HashMap::new();
            for &child in &element.children {
                let mut classes: Vec<&str> = elements[child].classes.iter().map(String::as_str).collect();
                classes.sort_unstable();
                let group = *keyed.entry((&elements[child].tag, classes)).or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
                groups[group].push(child);
                runs[child] = group;
            }
        }
        // Descendants come after their element, so a reverse pass sees all of an element's descendants first.
        let mut headings = vec![0; count];
        for i in (1..count).rev() {
            headings[i] += usize::from(heading_level(&elements[i].tag).is_some());
            headings[elements[i].parent] += headings[i];
        }
        let mut holds = vec![Holds::default(); count];
        let mut candidates: Vec<Option<Record>> = (0..count).map(|_| None).collect();
        for i in (1..count).rev() {
            let element = &elements[i];
            let tag = element.tag.as_str();
            let inside = holds[i];
            let own_heading = usize::from(heading_level(tag).is_some());
            // A table or frame is read in its own way, so neither one nor anything holding one is a card.
            let framed = matches!(tag, "table" | "iframe" | "frame");
            let record = self.repeated(i, &groups[runs[i]], &headings)
                && !framed
                && !inside.framed
                && headings[i] - own_heading <= 1
                && inside.block
                && !inside.item;
            let facts = if record { self.drawn_facts(i, &holds) } else { String::new() };
            if record && (element.text || !facts.is_empty()) {
                let link = if self.link(i) { Some(i) } else { inside.first_link.filter(|_| inside.links == 1) };
                let href = link.and_then(|link| elements[link].href.as_deref()).map(|href| join(&self.url, href));
                let kind = if tag == "li" { BlockKind::ListItem } else { BlockKind::Record };
                candidates[i] =
                    Some(Record { heading_level: inside.heading_level, close: element.close, kind, href, facts });
            }
            let own = Holds {
                framed,
                block: !element.hidden && displays_block(tag),
                // A repeated wrapper around another list is a group, so its inner records stay separately citable.
                item: candidates[i].is_some() || (tag == "li" && self.leaf(i)),
                links: usize::from(self.link(i)),
                first_link: self.link(i).then_some(i),
                leaf_of_its_own: LEAVES_OF_THEIR_OWN.contains(&tag),
                heading_level: (!element.hidden).then(|| heading_level(tag)).flatten(),
            };
            let mut around = inside;
            around.add(own);
            holds[element.parent].add(around);
        }
        // A card is read only where the capture's walk reaches it: not inside a block it reads whole, and not as a
        // heading, a table, a code block or a navigation menu, which it reads in their own ways.
        let mut reached = vec![true; count];
        let mut found = HashMap::new();
        for i in 1..count {
            let parent = elements[i].parent;
            let (tag, element) = (elements[parent].tag.as_str(), &elements[parent]);
            let whole = parent != 0
                && (element.hidden
                    || found.contains_key(&element.start)
                    || heading_level(tag).is_some()
                    || matches!(tag, "table" | "pre")
                    || (tag == "li" && self.leaf(parent)));
            reached[i] = reached[parent] && !whole;
            let element = &elements[i];
            let walked_into = element.tag == "nav" || element.navigation;
            if reached[i]
                && !walked_into
                && !element.hidden
                && let Some(record) = candidates[i].take()
            {
                found.insert(element.start, record);
            }
        }
        found
    }
}

/// One block of a fetched document, before its offsets are known.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub kind: BlockKind,
    pub text: String,
    pub heading_path: Vec<String>,
    pub href: Option<String>,
    /// Each link's text and address, in the order the block shows them, as `capture.js` lists them.
    pub links: Vec<(String, String)>,
    /// The frame a block was read from, when its frame's document is read into the page's.
    pub frame_id: Option<String>,
    /// The block is a card's whole text, what it only draws named after it, as `capture.js` marks a record.
    pub whole_record: bool,
}

/// A rendered frame: the document it shows, the number of blocks read before it, which is where its own go, and the
/// headings it stands under, which its own blocks sit under too.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub at: usize,
    pub document: FrameDocument,
    pub headings: Vec<String>,
}

/// What a frame shows: the address of its document, or the markup its `srcdoc` holds, which wins over a `src`.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameDocument {
    Address(String),
    Markup(String),
}

/// The served markup as blocks. `capture.js` reads the DOM a browser built, so it sees what CSS hid and what scripts
/// added; this sees the served markup only. It keeps the shapes the reader relies on anyway: headings with the path that names
/// what a value under them belongs to, one block per table row under its header, and an inline value left in the block
/// of its label.
struct Reader<'a> {
    url: &'a str,
    blocks: Vec<Found>,
    /// Rendered frames, whose documents hold text this markup does not.
    frames: Vec<Frame>,
    title: String,
    skip: usize,
    buffer: String,
    kind: BlockKind,
    href: Option<String>,
    headings: Vec<(usize, String)>,
    heading: Option<usize>,
    pre: usize,
    in_title: bool,
    tables: usize,
    /// Per open nested table, whether it sat in an outer cell, which reopens when it ends.
    outer_cells: Vec<bool>,
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    cell: Option<String>,
    cell_header: bool,
    header_row: Option<usize>,
    hidden: Vec<String>,
    controls: Vec<Control>,
    /// The control whose element is open, which its text labels.
    labelling: Option<(usize, String, String)>,
    /// An open `<li class="next">`, the pager markup most sites use instead of `rel="next"`; its link is the next page.
    pager_item: bool,
    /// Open pagination containers, by tag: a `nav`, or an element whose class or label names paging. A `next` class
    /// outside one marks a carousel, a wizard or a next article as often as a list's next page. Each counts the
    /// elements of its own tag open inside it, so an inner `</div>` closes the inner div, not the container.
    paging: Vec<(String, usize)>,
    /// Open dated elements, as tag, `datetime` and the text read inside so far. A relative date such as "3 weeks ago"
    /// holds the exact date only in this attribute, so the reader writes it after the text, as `capture.js` does.
    stamps: Vec<(String, String, String)>,
    /// The cards the first read found, by the tag token that opens each.
    records: HashMap<usize, Record>,
    /// The open card, whose whole text is one block.
    record: Option<Record>,
    /// The tag tokens opening footnote references, whose text is left out as `capture.js` leaves it out.
    notes: HashSet<usize>,
    /// The open link's address and the text read inside it so far.
    link: Option<(String, String)>,
    /// The links closed since the last block, as text and address, which the next block holds.
    links: Vec<(String, String)>,
    /// The links of the open table row, and of each row read so far, which each row's block holds.
    row_links: Vec<(String, String)>,
    rows_links: Vec<Vec<(String, String)>>,
}

impl Tags for Reader<'_> {
    fn start(&mut self, tag: &Tag, at: usize) {
        // An end tag a start tag implies can close the open card.
        if self.record.as_ref().is_some_and(|record| record.close == at) {
            self.close_record();
        }
        // `<svg/>` is foreign content, where a self-closing tag ends the element: it opens nothing to skip.
        if !(tag.self_closing && &*tag.name == "svg") {
            self.open(&tag.name, &tag.attrs, at);
        }
    }

    fn end(&mut self, name: &str, at: usize) {
        self.close(name);
        if self.record.as_ref().is_some_and(|record| record.close == at) {
            self.close_record();
        }
    }

    fn data(&mut self, data: &str) {
        if self.in_title {
            self.title.push_str(data);
            return;
        }
        if self.skip > 0 {
            return;
        }
        if let Some((_, _, text)) = &mut self.labelling {
            text.push_str(data);
        }
        for (_, _, text) in &mut self.stamps {
            text.push_str(data);
        }
        if let Some((_, text)) = &mut self.link {
            text.push_str(data);
        }
        self.add(data);
    }
}

impl<'a> Reader<'a> {
    fn new(url: &'a str, records: HashMap<usize, Record>, notes: HashSet<usize>) -> Reader<'a> {
        Reader {
            url,
            blocks: Vec::new(),
            frames: Vec::new(),
            title: String::new(),
            skip: 0,
            buffer: String::new(),
            kind: BlockKind::Paragraph,
            href: None,
            headings: Vec::new(),
            heading: None,
            pre: 0,
            in_title: false,
            tables: 0,
            outer_cells: Vec::new(),
            rows: Vec::new(),
            row: None,
            cell: None,
            cell_header: false,
            header_row: None,
            hidden: Vec::new(),
            controls: Vec::new(),
            labelling: None,
            pager_item: false,
            paging: Vec::new(),
            stamps: Vec::new(),
            records,
            record: None,
            notes,
            link: None,
            links: Vec::new(),
            row_links: Vec::new(),
            rows_links: Vec::new(),
        }
    }

    /// The control a start tag makes, as the served markup describes it.
    fn control(&mut self, tag: &str, attrs: &[Attribute]) {
        let get = |name: &str| attrs.iter().find(|a| &*a.name.local == name).map(|a| a.value.to_string());
        let kind = get("type").unwrap_or_default().to_ascii_lowercase();
        let (role, operations): (&str, &[Operation]) = match tag {
            "a" if get("href").is_some_and(|href| !href.is_empty()) => ("link", &[Operation::Click]),
            "button" => ("button", &[Operation::Click]),
            "select" => ("combobox", &[Operation::Select]),
            "textarea" => ("textbox", &[Operation::Fill]),
            "input" => match kind.as_str() {
                "hidden" => return,
                "submit" | "button" | "reset" | "image" => ("button", &[Operation::Click]),
                "checkbox" | "radio" => (kind.as_str(), &[Operation::Click]),
                "file" => ("button", &[Operation::Upload]),
                _ => ("textbox", &[Operation::Fill, Operation::Enter]),
            },
            _ => return,
        };
        // A browser offers no disabled control, as the snapshot leaves out `:disabled`, `aria-disabled="true"` and
        // `inert`. A disabled next link is no next page: the last page of GitHub's issue list keeps one pointing a page
        // past the end, and each page past it does the same.
        let disabled = get("aria-disabled").is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
            || get("inert").is_some()
            || (tag != "a" && get("disabled").is_some());
        if disabled {
            return;
        }
        let named = ["aria-label", "placeholder", "title", "value", "name"];
        let label = named.iter().find_map(|name| get(name).filter(|value| !clean(value).is_empty()));
        let id = format!("fetched-{}", self.controls.len());
        let mut control = Control::new(&id, role, &clean(&label.unwrap_or_default()), operations.iter().copied());
        control.context = self.headings.last().map(|(_, heading)| heading.clone());
        control.href = get("href").filter(|_| tag == "a").map(|href| join(self.url, &href));
        control.input_type = (tag == "input").then_some(kind.clone()).filter(|kind| !kind.is_empty());
        control.input_name = get("name");
        let names_next = |value: Option<String>| value.is_some_and(|v| v.split_whitespace().any(|v| v == "next"));
        let pager =
            tag == "a" && (names_next(get("class")) || std::mem::take(&mut self.pager_item)) && !self.paging.is_empty();
        // Only a same-origin link is a pager: a next page on another site is not more of this list.
        let origin = |url: &str| url::Url::parse(url).ok().map(|url| url.origin());
        let same_origin = control.href.as_deref().is_some_and(|href| origin(href) == origin(self.url));
        control.next_page = (same_origin && (names_next(get("rel")) || pager)).then_some(true);
        control.download = get("download").map(|_| true);
        if matches!(tag, "a" | "button") {
            self.labelling = Some((self.controls.len(), tag.to_string(), String::new()));
        }
        self.controls.push(control);
    }

    fn open(&mut self, tag: &str, attrs: &[Attribute], at: usize) {
        let get = |name: &str| attrs.iter().find(|a| &*a.name.local == name).map(|a| &*a.value);
        // Counted before any early return, as `end` sees the closing tag of every element, hidden or not.
        if let Some((open, nested)) = self.paging.last_mut()
            && open == tag
        {
            *nested += 1;
        }
        let shown = self.skip == 0 && get("hidden").is_none() && !hides(get("style").unwrap_or_default());
        if shown && matches!(tag, "iframe" | "frame") {
            let document = match (get("srcdoc"), get("src")) {
                (Some(markup), _) => Some(FrameDocument::Markup(markup.to_string())),
                (None, Some(src)) => Some(FrameDocument::Address(join(self.url, src))),
                (None, None) => None,
            };
            if let Some(document) = document {
                self.frames.push(Frame { at: self.blocks.len(), document, headings: self.path() });
            }
        }
        if SKIPPED.contains(&tag) {
            self.skip += 1;
            return;
        }
        if self.skip > 0 {
            return;
        }
        if tag == "title" {
            self.in_title = true;
            return;
        }
        if get("hidden").is_some() || hides(get("style").unwrap_or_default()) || self.notes.contains(&at) {
            // A browser never renders this subtree, so a value it holds is not one the user can see or quote. A
            // footnote reference is skipped the same way: it points at a note and is no part of the text it marks.
            self.hidden.push(tag.to_string());
            self.skip += 1;
            return;
        }
        // A `time` or a custom element dates the text it shows; the `datetime` of `ins` and `del` dates an edit.
        if (tag == "time" || tag.contains('-'))
            && let Some(datetime) = get("datetime").map(text::strip).filter(|datetime| !datetime.is_empty())
        {
            self.stamps.push((tag.to_string(), datetime.to_string(), String::new()));
        }
        let record = if self.record.is_none() && self.tables == 0 { self.records.remove(&at) } else { None };
        if let Some(record) = &record {
            self.flush(None, None, false);
            // An atomic card skips its internal heading, which must still end a preceding sibling's heading.
            if let Some(level) = record.heading_level {
                self.headings.retain(|(higher, _)| *higher < level);
            }
        }
        self.control(tag, attrs);
        if tag == "a"
            && let Some(href) = get("href").filter(|href| !href.is_empty()).map(|href| join(self.url, href))
            && !href.starts_with("javascript:")
        {
            // A script link runs code here and leads nowhere to report.
            self.link = Some((href, String::new()));
        }
        // Pagination's own words, not "pag" alone: a `<body class="page">` layout class held every carousel open.
        let pages = |value: Option<&str>| {
            value.map(|value| value.to_ascii_lowercase()).is_some_and(|v| v.contains("pagin") || v.contains("pager"))
        };
        if tag == "nav" || pages(get("class")) || pages(get("aria-label")) {
            if let Some((open, nested)) = self.paging.last_mut()
                && open == tag
            {
                // Counted above as nested, it is a container of its own.
                *nested -= 1;
            }
            self.paging.push((tag.to_string(), 0));
        }
        if tag == "li" {
            self.pager_item = get("class").is_some_and(|class| class.split_whitespace().any(|class| class == "next"));
        }
        if self.record.is_some() {
            self.record_break(tag, true);
            return;
        }
        if let Some(record) = record {
            self.record = Some(record);
            return;
        }
        if tag == "br" {
            self.add("\n");
            return;
        }
        if let Some(level) = heading_level(tag) {
            self.flush(None, None, false);
            self.heading = Some(level);
            self.kind = BlockKind::Heading;
            return;
        }
        if tag == "table" {
            self.flush(None, None, false);
            if self.tables > 0 {
                // A table inside a cell lays out a page around its own rows, as Hacker News lists every story in one
                // cell of its page table. Read flat, the whole list was one row; the rows so far end here, and the
                // inner table's rows are rows.
                self.outer_cells.push(self.cell.is_some());
                self.close_cell();
                self.close_row();
                self.emit_table();
            }
            self.tables += 1;
            self.rows.clear();
            self.header_row = None;
            return;
        }
        if self.tables > 0 {
            self.table_start(tag, get("alt").unwrap_or_default());
            return;
        }
        if tag == "li" {
            self.flush(None, None, false);
            self.kind = BlockKind::ListItem;
            return;
        }
        if tag == "pre" {
            self.flush(None, None, false);
            self.pre += 1;
            return;
        }
        if tag == "img"
            && self.cell.is_none()
            && let Some(alt) = get("alt").filter(|alt| !alt.is_empty())
        {
            // An icon or a flag states its value only in its alt text, which no other reader would quote.
            self.add(&format!(" [{}]", clean(alt)));
            return;
        }
        if tag == "a"
            && let Some(href) = get("href").filter(|href| !href.is_empty())
            && clean(&self.buffer).is_empty()
        {
            // A link whose own text is the whole block keeps its destination, which is the one thing an answer
            // about addresses has to cite.
            self.href = Some(join(self.url, href));
            return;
        }
        if BLOCKS.contains(&tag) {
            self.flush(None, None, false);
        }
    }

    fn close(&mut self, tag: &str) {
        if tag == "li" {
            self.pager_item = false;
        }
        if let Some((open, nested)) = self.paging.last_mut()
            && open == tag
        {
            if *nested == 0 {
                self.paging.pop();
            } else {
                *nested -= 1;
            }
        }
        if let Some((index, _, text)) = self.labelling.take_if(|(_, open, _)| open == tag) {
            // What the element says for itself names it before any attribute does.
            let text = clean(&text);
            if !text.is_empty() {
                self.controls[index].label = text;
            }
        }
        if self.hidden.last().is_some_and(|hidden| hidden == tag) {
            self.hidden.pop();
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        if SKIPPED.contains(&tag) {
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        if self.skip > 0 {
            return;
        }
        if tag == "title" {
            self.in_title = false;
            return;
        }
        if tag == "a"
            && let Some((href, text)) = self.link.take()
        {
            let text = clean(&text);
            if !text.is_empty() {
                if self.cell.is_some() { self.row_links.push((text, href)) } else { self.links.push((text, href)) }
            }
        }
        if let Some((_, datetime, text)) = self.stamps.pop_if(|(open, _, _)| open == tag) {
            let shown = clean(&text);
            if shown.is_empty() {
                self.add(&datetime);
            } else if !shown.contains(&datetime) {
                self.add(&format!(" ({datetime})"));
            }
        }
        if self.record.is_some() {
            self.record_break(tag, false);
            return;
        }
        if heading_level(tag).is_some() {
            self.flush(None, None, true);
            self.heading = None;
            self.kind = BlockKind::Paragraph;
            return;
        }
        if tag == "table" && self.tables > 0 {
            self.close_cell();
            self.close_row();
            self.emit_table();
            self.tables -= 1;
            if self.outer_cells.pop() == Some(true) {
                // The outer cell goes on after the inner table, and what it says next stays a cell of its own row.
                self.cell = Some(String::new());
                self.cell_header = false;
            }
            return;
        }
        if self.tables > 0 {
            if tag == "td" || tag == "th" {
                self.close_cell();
            } else if tag == "tr" {
                self.close_cell();
                self.close_row();
            }
            return;
        }
        if tag == "pre" {
            self.pre = self.pre.saturating_sub(1);
            self.flush(None, None, false);
            return;
        }
        if tag == "li" {
            self.flush(None, None, false);
            self.kind = BlockKind::Paragraph;
            return;
        }
        if tag == "a"
            && let Some(href) = self.href.take()
        {
            self.flush(Some(BlockKind::Link), Some(href), false);
            return;
        }
        if BLOCKS.contains(&tag) {
            self.flush(None, None, false);
        }
    }

    fn add(&mut self, data: &str) {
        match &mut self.cell {
            Some(cell) => cell.push_str(data),
            // A card's lines are its blocks, as `innerText` writes them, so the markup's own newlines are spaces.
            None if self.record.is_some() => {
                self.buffer.extend(data.chars().map(|c| if c.is_ascii_whitespace() { ' ' } else { c }));
            }
            None => self.buffer.push_str(data),
        }
    }

    /// The line breaks `innerText` writes at a tag inside a card: a `br` its own, and around a block as many as it
    /// requires, two for a paragraph, which the breaks already written count towards.
    fn record_break(&mut self, tag: &str, start: bool) {
        let wanted = match tag {
            "br" if start => {
                self.buffer.truncate(self.buffer.trim_end_matches(' ').len());
                self.buffer.push('\n');
                return;
            }
            "p" => 2,
            tag if tag != "br" && displays_block(tag) => 1,
            _ => return,
        };
        self.buffer.truncate(self.buffer.trim_end_matches(' ').len());
        let written = self.buffer.len() - self.buffer.trim_end_matches('\n').len();
        for _ in written..wanted {
            self.buffer.push('\n');
        }
    }

    /// The open card as one block, with what it only draws after its text.
    fn close_record(&mut self) {
        let Some(record) = self.record.take() else { return };
        self.buffer.truncate(self.buffer.trim_end_matches(|c: char| c.is_ascii_whitespace()).len());
        self.buffer.push_str(&record.facts);
        let before = self.blocks.len();
        self.flush(Some(record.kind), record.href, false);
        if let Some(block) = self.blocks.get_mut(before) {
            block.whole_record = true;
        }
    }

    fn table_start(&mut self, tag: &str, alt: &str) {
        if tag == "tr" {
            self.close_cell();
            self.close_row();
            self.row = Some(Vec::new());
            return;
        }
        if tag == "td" || tag == "th" {
            self.close_cell();
            self.cell = Some(String::new());
            self.cell_header = tag == "th";
            return;
        }
        if tag == "img"
            && !alt.is_empty()
            && let Some(cell) = &mut self.cell
        {
            // A cell can state its value with a picture alone, the way a winner's flag does.
            cell.push_str(&format!(" [{}]", clean(alt)));
        }
    }

    fn close_cell(&mut self) {
        let Some(cell) = self.cell.take() else { return };
        self.row.get_or_insert_with(Vec::new).push(clean(&cell));
        if self.cell_header && self.header_row.is_none() {
            self.header_row = Some(self.rows.len());
        }
    }

    fn close_row(&mut self) {
        let links = std::mem::take(&mut self.row_links);
        if let Some(row) = self.row.take().filter(|row| !row.is_empty()) {
            self.rows.push(row);
            self.rows_links.push(links);
        }
    }

    fn emit_table(&mut self) {
        let path = self.path();
        let mut links = std::mem::take(&mut self.rows_links).into_iter();
        for (index, cells) in std::mem::take(&mut self.rows).into_iter().enumerate() {
            let line = cells.iter().map(|cell| cell.replace('|', "\\|")).collect::<Vec<_>>().join(" | ");
            let mut text = format!("| {line} |");
            if index == 0 && self.header_row == Some(0) {
                // Without the divider the row above reads as prose, and a reader cannot tell a column name from a
                // value. It shares the header's block, as a browser capture writes it, so a block of dashes is a row.
                text.push_str(&format!("\n| {} |", vec!["---"; cells.len()].join(" | ")));
            }
            self.blocks.push(Found {
                kind: BlockKind::Table,
                text,
                heading_path: path.clone(),
                href: None,
                links: links.next().unwrap_or_default(),
                frame_id: None,
                whole_record: false,
            });
        }
        self.row = None;
        self.header_row = None;
    }

    fn path(&self) -> Vec<String> {
        self.headings.iter().map(|(_, name)| name.clone()).collect()
    }

    fn flush(&mut self, kind: Option<BlockKind>, href: Option<String>, closing_heading: bool) {
        let raw = std::mem::take(&mut self.buffer);
        let links = std::mem::take(&mut self.links);
        if let Some(cell) = &mut self.cell {
            // Text of a block inside a cell belongs to that cell's row, not to a block of its own.
            cell.push_str(&raw);
            return;
        }
        // A pre block keeps its line breaks, which are its content; everything else is read as prose.
        let text = if self.pre > 0 {
            if text::strip(&raw).is_empty() {
                String::new()
            } else {
                raw.trim_matches('\n').trim_end_matches(text::is_space).to_string()
            }
        } else {
            clean(&raw)
        };
        if text.is_empty() {
            if closing_heading && let Some(level) = self.heading {
                self.headings.retain(|(higher, _)| *higher < level);
            }
            return;
        }
        let kind = kind.unwrap_or(self.kind);
        self.blocks.push(Found {
            kind,
            text: text.clone(),
            heading_path: self.path(),
            href,
            links,
            frame_id: None,
            whole_record: false,
        });
        if closing_heading && let Some(level) = self.heading {
            // A heading names what follows it, never itself, and a lower heading ends every higher one.
            self.headings.retain(|(higher, _)| *higher < level);
            self.headings.push((level, text));
        }
    }
}

/// `href` resolved against `base` as WHATWG resolves it (docs/adr/0002); an address that does not resolve is kept as
/// written.
pub fn join(base: &str, href: &str) -> String {
    url::Url::parse(base).and_then(|base| base.join(href)).map_or_else(|_| href.to_string(), String::from)
}

/// The document's title and its blocks, in reading order.
pub fn blocks(html: &str, url: &str) -> (String, Vec<Found>) {
    let (title, found, ..) = read(html, url);
    (title, found)
}

/// The document's title, its blocks in reading order, the controls its markup offers, and the rendered frames that
/// hold documents of their own.
pub fn read(html: &str, url: &str) -> (String, Vec<Found>, Vec<Control>, Vec<Frame>) {
    // A card is told by its siblings, which come after it, so the tree is read before the text.
    let mut tree = tokenize(html, Sink::new(Tree::new(url))).into_inner();
    let notes = tree.note_references();
    let mut reader = tokenize(html, Sink::new(Reader::new(url, tree.records(), notes))).into_inner();
    reader.close_record();
    reader.flush(None, None, false);
    (clean(&reader.title), reader.blocks, reader.controls, reader.frames)
}

/// The first `<meta>` charset in a document's first 1024 bytes, which is where the HTML standard's prescan looks for
/// one when the response names none. UTF-16 named here reads as UTF-8, since a document that says so in ASCII is not.
pub fn meta_charset(body: &[u8]) -> Option<&'static Encoding> {
    struct Meta(RefCell<Option<&'static Encoding>>);
    impl TokenSink for Meta {
        type Handle = ();
        fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
            if let Token::TagToken(tag) = token
                && tag.kind == TagKind::StartTag
                && &*tag.name == "meta"
                && self.0.borrow().is_none()
            {
                let get = |name: &str| tag.attrs.iter().find(|a| &*a.name.local == name).map(|a| a.value.to_string());
                let label = get("charset").or_else(|| {
                    let content_type = get("http-equiv").is_some_and(|v| v.eq_ignore_ascii_case("content-type"));
                    content_type.then(|| get("content").and_then(|content| charset_parameter(&content))).flatten()
                });
                *self.0.borrow_mut() = label.and_then(|label| Encoding::for_label(label.as_bytes()));
            }
            TokenSinkResult::Continue
        }
    }
    let head = &body[..body.len().min(1024)];
    let (ascii, _, _) = encoding_rs::WINDOWS_1252.decode(head);
    let found = tokenize(&ascii, Meta(RefCell::new(None))).0.into_inner()?;
    Some(if found == encoding_rs::UTF_16BE || found == encoding_rs::UTF_16LE { UTF_8 } else { found })
}

/// The `charset` parameter of a `Content-Type` value, unquoted.
pub fn charset_parameter(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['"', '\'']).to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_collapses_what_a_browser_would() {
        assert_eq!(clean("  a \t b \n  c\n\n\n\nd  "), "a b\nc\n\nd");
        assert_eq!(clean("a\u{a0}b"), "a\u{a0}b");
    }

    #[test]
    fn a_row_with_no_closing_tag_still_reaches_the_table() {
        let (_, found) = blocks("<table><tr><td>a<td>1<tr><td>b<td>2</table>", "https://a.example/");
        let texts: Vec<&str> = found.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["| a | 1 |", "| b | 2 |"]);
    }

    #[test]
    fn two_tables_of_one_class_under_their_own_headings_are_tables() {
        let table = "<table class='sortable'><tr><th>Due</th></tr><tr><td>$50.00</td></tr></table>";
        let (_, found) =
            blocks(&format!("<div class='example'><h4>One</h4>{table}<h4>Two</h4>{table}</div>"), "https://a.example/");
        let kinds: Vec<BlockKind> = found.iter().map(|b| b.kind).collect();
        assert_eq!(kinds.iter().filter(|kind| **kind == BlockKind::Table).count(), 4, "{found:?}");
        assert!(!kinds.contains(&BlockKind::Record), "{found:?}");
    }

    #[test]
    fn a_rendered_frame_is_kept_with_its_document_and_place() {
        let (_, found, _, frames) = read(
            "<h2>Internal frame</h2><iframe src=/inner></iframe><iframe srcdoc='<p>x'></iframe>\
             <iframe></iframe><iframe hidden src=/a></iframe><div style='display:none'><iframe src=/b></iframe></div>\
             <noscript><iframe src=/c></iframe></noscript>",
            "https://a.example/",
        );
        assert_eq!(found.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(), ["Internal frame"]);
        assert_eq!(
            frames,
            [
                Frame {
                    at: 1,
                    document: FrameDocument::Address("https://a.example/inner".into()),
                    headings: vec!["Internal frame".into()],
                },
                Frame {
                    at: 1,
                    document: FrameDocument::Markup("<p>x".into()),
                    headings: vec!["Internal frame".into()]
                },
            ]
        );
    }

    /// The markup of `capture.js`'s footnote test, read without a browser: the references go as they go there (#862).
    #[test]
    fn a_footnote_reference_is_not_read_as_part_of_the_text_it_marks() {
        let (_, found) = blocks(
            r##"<div id="top"><p><a href="#history">[History]</a></p>
            <table><tr><th>Population</th>
            <td>41,798,407<sup id="cite_ref-3" class="reference"><a href="#cite_note-3"><span>[</span>3<span>]</span></a></sup> (<a href="/rank">37th</a>)</td></tr>
            <tr><th>Completed</th><td>31 March 1889<sup id="cite_ref-:0_1-0"><a href="#cite_note-%3A0-1">[1]</a></sup></td></tr></table>
            <p>Dune is a novel.<sup><a href="#fn1" role="doc-noteref">2</a></sup> It sold well.</p></div>
            <h2 id="history">History</h2><p><a href="#top">Back to top</a></p>
            <ol><li id="cite_note-3"><a href="#cite_ref-3">^</a> Statistics Canada.</li>
            <li id="cite_note-:0-1"><a href="#cite_ref-:0_1-0"><sup>a</sup></a> Tour Eiffel.</li></ol>"##,
            "https://a.example/",
        );
        let read: Vec<&str> = found.iter().map(|b| b.text.as_str()).collect();
        assert!(read.iter().any(|text| text.starts_with("| Population | 41,798,407 (37th) |")), "{read:?}");
        assert!(read.iter().any(|text| text.ends_with("| Completed | 31 March 1889 |")), "{read:?}");
        assert!(read.contains(&"Dune is a novel. It sold well."), "{read:?}");
        assert!(read.contains(&"[History]") && read.contains(&"Back to top"), "{read:?}");
        assert!(read.iter().any(|text| text.contains("Statistics Canada.")), "{read:?}");
    }

    #[test]
    fn an_element_hides_itself_only_by_its_own_declaration() {
        assert!(hides("color:red; Display : None"));
        assert!(hides("visibility:hidden"));
        assert!(!hides("display: block"));
    }

    #[test]
    fn raw_text_and_references_read_as_a_browser_reads_them() {
        let (title, found) = blocks(
            "<title>A &amp; <b>B</title><script>if (a<b) x = '&amp;</div>'</SCRIPT >\
             <p data-x=1>x &amp; y &copy</p><!-- c > d --><p>1 < 2</p>",
            "https://a.example/",
        );
        assert_eq!(title, "A & <b>B");
        let texts: Vec<&str> = found.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["x & y \u{a9}", "1 < 2"]);
    }

    #[test]
    fn the_markup_names_its_links_and_fields() {
        let (_, _, controls, _) = read(
            "<h2>Pages</h2><a href='/b' rel='next'>More <b>items</b></a><a href='/c'><img src=x></a>\
             <nav><ul class='pager'><li class='next'><a href='/e'>Next</a></li><li><a href='/f'>Other</a></li></ul></nav>\
             <div hidden><a href='/secret'>x</a></div><table><tr><td><a href=/d>In a row</a></table>\
             <form><input type=hidden name=t><input name=q placeholder='Search'><button>Go</button></form>",
            "https://a.example/list",
        );
        let shown: Vec<(&str, &str, Option<&str>)> =
            controls.iter().map(|c| (c.role.as_str(), c.label.as_str(), c.href.as_deref())).collect();
        assert_eq!(
            shown,
            [
                ("link", "More items", Some("https://a.example/b")),
                ("link", "", Some("https://a.example/c")),
                ("link", "Next", Some("https://a.example/e")),
                ("link", "Other", Some("https://a.example/f")),
                ("link", "In a row", Some("https://a.example/d")),
                ("textbox", "Search", None),
                ("button", "Go", None),
            ]
        );
        let next: Vec<_> = controls.iter().map(|c| c.next_page).collect();
        assert_eq!(next, [Some(true), None, Some(true), None, None, None, None]);
        assert_eq!(controls[0].context.as_deref(), Some("Pages"));
    }

    #[test]
    fn a_disabled_control_is_not_offered_and_a_disabled_next_link_is_no_next_page() {
        // The last page of GitHub's issue list, as served: its Next link is disabled but still points one page past
        // the end, and every page past it does the same (#863).
        let (_, _, controls, _) = read(
            "<nav aria-label='Pagination'><a href='/issues?page=1' rel='prev'>Previous</a>\
             <a rel='next' aria-hidden='true' aria-disabled='true' href='/issues?page=3'>Next</a></nav>\
             <form><button disabled>Send</button><input name=q disabled><button>Go</button>\
             <a href='/x' inert>Inert</a><a href='/y' aria-disabled='false'>Enabled</a></form>",
            "https://a.example/issues?page=2",
        );
        let shown: Vec<&str> = controls.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(shown, ["Previous", "Go", "Enabled"]);
        assert!(controls.iter().all(|c| c.next_page.is_none()), "{controls:?}");
    }

    #[test]
    fn a_nested_element_of_the_containers_tag_does_not_close_the_pagination() {
        let (_, _, controls, _) = read(
            "<div class='pagination'><div>Page 1 of 3</div><a class='next' href='/page/2'>Next</a></div>",
            "https://a.example/list",
        );
        assert_eq!(controls.len(), 1);
        assert_eq!(controls[0].next_page, Some(true));
    }

    #[test]
    fn a_next_class_marks_a_pager_only_inside_pagination_on_this_site() {
        let (_, _, controls, _) = read(
            "<div class='carousel'><a class='next' href='/slide/2'>Next</a></div>\
             <article><li class='next'><a href='/post/9'>Next article</a></li></article>\
             <nav aria-label='Pagination'><a class='next' href='https://other.example/page/2'>Next</a>\
             <a rel='next' href='https://other.example/page/3'>More</a><a class='next' href='/page/2'>Next</a></nav>\
             <div class='Paginator'><li class='next'><a href='/page/3'>Older</a></li></div>",
            "https://a.example/list",
        );
        let next: Vec<_> = controls.iter().map(|c| c.next_page).collect();
        assert_eq!(next, [None, None, None, None, Some(true), Some(true)]);
    }

    #[test]
    fn review_a_page_layout_class_does_not_turn_a_carousel_into_pagination() {
        let (_, _, controls, _) = read(
            "<body class='page'><main><div class='carousel'>\
             <a class='next' href='/slide/2'>Next slide</a></div></main></body>",
            "https://a.example/list",
        );
        assert_eq!(controls.len(), 1);
        assert_eq!(controls[0].next_page, None, "a page layout class is not a pagination container");
    }

    #[test]
    fn a_time_keeps_its_machine_date() {
        let text = |html: &str| {
            let (_, found) = blocks(html, "https://a.example/");
            found.into_iter().map(|b| b.text).collect::<Vec<_>>()
        };
        assert_eq!(
            text(
                "<p>rustbot released this <relative-time datetime=\"2026-10-16T04:47:44Z\"> 16 Oct 04:47 </relative-time></p>"
            ),
            ["rustbot released this 16 Oct 04:47 (2026-10-16T04:47:44Z)"]
        );
        assert_eq!(text("<p><time datetime=\"2026-01-02\">2026-01-02</time></p>"), ["2026-01-02"]);
        assert_eq!(text("<p><del datetime=\"2020-01-01\">x</del></p>"), ["x"]);
        assert_eq!(text("<p><local-time datetime=\"2026-05-05\"></local-time></p>"), ["2026-05-05"]);
        assert_eq!(
            text("<table><tr><td><time datetime=\"2026-03-04\">March</time></td></tr></table>"),
            ["| March (2026-03-04) |"]
        );
        let (_, found, controls, _) =
            read("<p><a href=\"/c\"><time datetime=\"2026-11-01\">5 days ago</time></a></p>", "https://a.example/");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, BlockKind::Link);
        assert_eq!(found[0].text, "5 days ago (2026-11-01)");
        assert_eq!(controls[0].label, "5 days ago");
    }

    fn kinds_and_texts(html: &str) -> Vec<(BlockKind, String)> {
        blocks(html, "https://a.example/").1.into_iter().map(|b| (b.kind, b.text)).collect()
    }

    #[test]
    fn a_repeated_quote_card_is_one_record() {
        let card = |n: usize| {
            format!(
                "<div class=\"quote\">\n  <span class=\"text\">Quote {n}.</span>\n  <span>by <small class=\"author\">\
                 Author {n}</small>\n  <a href=\"/author/{n}\">(about)</a></span>\n  <div class=\"tags\">\n    Tags:\n    \
                 <meta class=\"keywords\" content=\"a\">\n    <a class=\"tag\" href=\"/tag/a\">a</a>\n    \
                 <a class=\"tag\" href=\"/tag/b\">b</a>\n  </div>\n</div>\n"
            )
        };
        let html = format!("<h1>Quotes</h1><div class=\"col\">{}{}{}</div>", card(1), card(2), card(3));
        let (_, found) = blocks(&html, "https://a.example/");
        let records: Vec<_> = found.iter().filter(|b| b.kind == BlockKind::Record).collect();
        let texts: Vec<&str> = records.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, (1..=3).map(|n| format!("Quote {n}. by Author {n} (about)\nTags: a b")).collect::<Vec<_>>(),);
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(records.iter().all(|b| b.heading_path == ["Quotes"] && b.href.is_none()));
    }

    #[test]
    fn a_repeated_product_card_is_one_block() {
        let card = |n: usize| {
            format!(
                "<li class=\"col\"><article class=\"product_pod\"><p class=\"star-rating Three\"><i class=\"star\"></i>\
                 <i class=\"star\"></i></p><h3><a href=\"/book/{n}\">Title {n}</a></h3>\
                 <p class=\"price_color\">\u{a3}51.7{n}</p><p class=\"availability\">In stock</p></article></li>"
            )
        };
        let html = format!("<h2>Books</h2><ol class=\"row\">{}{}{}</ol><p>After</p>", card(1), card(2), card(3));
        let (_, found) = blocks(&html, "https://a.example/");
        // As `capture.js` writes a list item that is a card: one list item holding the whole card.
        let cards: Vec<_> = found.iter().filter(|b| b.kind == BlockKind::ListItem).collect();
        assert_eq!(
            cards.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(),
            (1..=3)
                .map(|n| format!("Title {n}\n\n\u{a3}51.7{n}\n\nIn stock [star-rating Three: no text]"))
                .collect::<Vec<_>>(),
        );
        assert_eq!(cards[0].href.as_deref(), Some("https://a.example/book/1"));
        // A card that is an `li` is still the whole card, as `capture.js` marks it (#992); the paragraph after is not.
        assert!(cards.iter().all(|b| b.whole_record) && !found.last().unwrap().whole_record, "{found:?}");
        assert_eq!(cards[0].heading_path, ["Books"], "the card's own heading names nothing after it");
        assert_eq!(
            found.last().map(|b| (b.text.as_str(), b.heading_path.clone())),
            Some(("After", vec!["Books".into()]))
        );
    }

    #[test]
    fn sibling_records_do_not_inherit_an_earlier_records_heading() {
        for level in [3, 5] {
            let html = include_str!("../../../fastbrowse-browser/tests/fixtures/sibling-record-headings.html")
                .replace("<h3>", &format!("<h{level}>"))
                .replace("</h3>", &format!("</h{level}>"));
            let (_, found) = blocks(&html, "https://example.test/releases");
            let records: Vec<_> = found.iter().filter(|block| block.whole_record).collect();
            assert_eq!(records.len(), 3);
            for (record, version) in records.iter().zip(["3.0", "2.0", "1.0"]) {
                assert!(record.text.contains(&format!("Version {version}")));
                assert_eq!(record.heading_path, ["Package", "Release history"]);
            }
            let after = found.iter().find(|block| block.text == "After the releases.").unwrap();
            assert_eq!(after.heading_path, ["Package", "Release history"]);
        }
    }

    #[test]
    fn record_heading_boundaries_follow_visible_heading_ranks() {
        let cases = [
            ("<h2>Group</h2><h3>Earlier</h3><h3></h3><p>After</p>".to_string(), vec!["Group"]),
            (
                format!("<h2>Group</h2>{}<p>After</p>", "<div class='card'><h2></h2><p>Value</p></div>".repeat(3)),
                vec![],
            ),
            ("<h2>Group</h2><h4>Earlier</h4><h3>Later</h3><p>After</p>".to_string(), vec!["Group", "Later"]),
            (
                format!("<h2>Group</h2>{}<p>After</p>", "<div class='card'><h2>Item</h2><p>Value</p></div>".repeat(3)),
                vec![],
            ),
            (
                format!(
                    "<h2>Group</h2>{}<p>After</p>",
                    "<div class='card'><span hidden><h2>Hidden</h2></span><p>Value</p></div>".repeat(3)
                ),
                vec!["Group"],
            ),
        ];
        for (markup, expected) in cases {
            let (_, found) = blocks(&markup, "https://example.test/");
            let after = found.iter().find(|block| block.text == "After").unwrap();
            assert_eq!(after.heading_path, expected, "{markup}");
        }
        let markup = format!(
            "<h2>Group</h2><div><h3>Earlier</h3><p>Value</p></div>{}",
            "<a class='card' href='/item'><h3>Item</h3><p>Value</p></a>".repeat(3)
        );
        let (_, _, controls, _) = read(&markup, "https://example.test/");
        assert_eq!(controls.len(), 3);
        assert!(controls.iter().all(|control| control.context.as_deref() == Some("Group")));
    }

    #[test]
    fn two_cards_without_a_heading_around_them_are_not_records() {
        let found = kinds_and_texts(&"<div class='card'><p>First</p><p>Second</p></div>".repeat(2));
        assert!(found.iter().all(|(kind, _)| *kind == BlockKind::Paragraph), "{found:?}");
        assert_eq!(found.len(), 4);
        // A pair is a record when a heading outside both names their group, or when each links somewhere else.
        let headed = kinds_and_texts(&format!(
            "<section><h2>Pair</h2>{}</section>",
            "<div class='card'><p>A</p></div>".repeat(2)
        ));
        assert_eq!(headed.iter().filter(|(kind, _)| *kind == BlockKind::Record).count(), 2, "{headed:?}");
        let linked = kinds_and_texts(
            "<div><a class='card' href='/one'><div><p>First item</p><span>Type</span></div></a>\
             <a class='card' href='/two'><div><p>Second item</p><span>Type</span></div></a></div>",
        );
        assert_eq!(
            linked,
            [(BlockKind::Record, "First item\n\nType".into()), (BlockKind::Record, "Second item\n\nType".into())]
        );
    }

    #[test]
    fn a_repeated_wrapper_around_a_list_is_a_group_of_its_items() {
        let group = "<div class='group'><p>Collection</p><ul><li>One</li><li>Two</li><li>Three</li></ul></div>";
        let found = kinds_and_texts(&group.repeat(3));
        assert!(found.iter().all(|(kind, _)| *kind != BlockKind::Record), "{found:?}");
        assert_eq!(found.iter().filter(|(kind, _)| *kind == BlockKind::ListItem).count(), 9);
        let cards = "<article><p>Quotation</p><p>by Author</p></article>".repeat(3);
        let nested =
            kinds_and_texts(&format!("<div class='g'><p>Collection</p><section>{cards}</section></div>").repeat(3));
        let records: Vec<_> = nested.iter().filter(|(kind, _)| *kind == BlockKind::Record).collect();
        assert_eq!(records.len(), 9);
        assert!(records.iter().all(|(_, text)| text == "Quotation\n\nby Author"));
    }

    #[test]
    fn a_card_is_not_a_record_where_capture_reads_it_otherwise() {
        for (id, html) in [
            ("inline-only", "<div class='card'><span>Inline only</span></div>".repeat(3)),
            ("two-headings", "<div class='card'><h2>First</h2><h3>Second</h3><p>Body</p></div>".repeat(3)),
            ("table", "<div class='card'><p>Out</p><table><tr><td>In</td></tr></table></div>".repeat(3)),
            ("classless", "<div><p>Layout</p><p>column</p></div>".repeat(3)),
            ("in-a-table", format!("<table><tr><td>{}</td></tr></table>", "<div class='c'><p>x</p></div>".repeat(3))),
            ("hidden", "<div class='card' hidden><p>Secret</p></div>".repeat(3)),
        ] {
            let found = kinds_and_texts(&html);
            assert!(found.iter().all(|(kind, _)| *kind != BlockKind::Record), "{id}: {found:?}");
        }
        // An item's end tag a browser implies still ends the card.
        let implied = kinds_and_texts("<ul><li class=c><p>A<p>a<li class=c><p>B<p>b<li class=c><p>C<p>c</ul><p>After");
        assert_eq!(
            implied.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>(),
            ["A\n\na", "B\n\nb", "C\n\nc", "After"]
        );
    }

    #[test]
    fn a_meta_charset_names_the_encoding() {
        assert_eq!(meta_charset(b"<meta charset=\"shift_jis\"><p>x"), Some(encoding_rs::SHIFT_JIS));
        let equiv = b"<meta http-equiv=Content-Type content='text/html; charset=windows-1251'>";
        assert_eq!(meta_charset(equiv), Some(encoding_rs::WINDOWS_1251));
        assert_eq!(meta_charset(b"<meta charset=utf-16le>"), Some(UTF_8));
        assert_eq!(meta_charset(b"<p>no meta</p>"), None);
    }
}
