//! TeX math → plain Unicode, so `\Delta S^\circ = \sum \nu_i S_{\text{products}}` reads as `ΔS° = ∑ νᵢ S_products` in a
//! terminal that can't typeset it.
//!
//! Nemotron writes math in LaTeX: `\[ … \]` and `$$ … $$` for display math, `\( … \)` and `$ … $` inline. This runs on
//! the Markdown before it's parsed, because the parser would otherwise eat the backslashes in `\[`, `\(`, `\,` and
//! `\;` and turn subscript underscores into italics. Code spans and code blocks are left alone.

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

/// Around the contents of `\boxed{…}` in `tex_to_unicode`'s output, until it's placed as display or inline math.
const BOX_OPEN: char = '\u{1}';
const BOX_CLOSE: char = '\u{2}';

/// Replace every math region in `markdown` with readable Unicode. Unclosed delimiters (a reply still streaming) are
/// left as they are.
pub fn render_math(markdown: &str) -> Cow<'_, str> {
    if !markdown.contains(['\\', '$']) {
        return Cow::Borrowed(markdown);
    }
    let mut out = String::with_capacity(markdown.len());
    let mut fence: Option<String> = None;
    let mut prose = String::new();
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(open) = &fence {
            out.push_str(line);
            if trimmed.starts_with(open.as_str()) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            out.push_str(&convert_prose(&std::mem::take(&mut prose)));
            fence = Some(trimmed.chars().take_while(|c| *c == '`' || *c == '~').collect());
            out.push_str(line);
            continue;
        }
        prose.push_str(line);
    }
    out.push_str(&convert_prose(&prose));
    Cow::Owned(out)
}

/// Convert the math in text with no code blocks (inline code spans are skipped).
fn convert_prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        // A code span: copy it through, up to the matching run of backticks.
        if rest.starts_with('`') {
            let ticks = rest.len() - rest.trim_start_matches('`').len();
            let fence = &rest[..ticks];
            let after = &rest[ticks..];
            let end = after.find(fence).map_or(rest.len(), |i| ticks + i + ticks);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        if let Some((math, display, len)) = math_at(rest, &out) {
            let converted = tex_to_unicode(math);
            if display {
                place_display(&mut out, &converted);
            } else {
                out.push_str(&emphasize_boxes(&escape(&converted)));
            }
            rest = &rest[len..];
            continue;
        }
        let ch = rest.chars().next().unwrap();
        // An escaped character (`\$`, `\\`) is ordinary text; keep it escaped for the Markdown parser.
        if ch == '\\' {
            if let Some(next) = rest[1..].chars().next().filter(|c| matches!(c, '$' | '\\' | '`')) {
                out.push('\\');
                out.push(next);
                rest = &rest[1 + next.len_utf8()..];
                continue;
            }
        }
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// The math region starting at the beginning of `text`: (its TeX, whether it's display math, bytes consumed).
fn math_at<'a>(text: &'a str, before: &str) -> Option<(&'a str, bool, usize)> {
    for (open, close, display) in [("\\[", "\\]", true), ("$$", "$$", true), ("\\(", "\\)", false)] {
        if let Some(body) = text.strip_prefix(open) {
            let end = body.find(close)?;
            return Some((&body[..end], display, open.len() + end + close.len()));
        }
    }
    // `$…$` follows Pandoc's rule so prices don't count: no space just inside either `$`, the closing one isn't
    // followed by a digit, and it stays on one line. A `$` right after a letter or digit isn't an opening one.
    let body = text.strip_prefix('$')?;
    if body.starts_with([' ', '\t', '\n']) || before.ends_with(|c: char| c.is_alphanumeric()) {
        return None;
    }
    let end = body.find(['$', '\n']).filter(|&i| body.as_bytes()[i] == b'$')?;
    let math = &body[..end];
    let after = &body[end + 1..];
    if math.is_empty() || math.ends_with([' ', '\t']) || after.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((math, false, 1 + end + 1))
}

/// Display math on lines of its own, indented, with hard breaks so neighbouring lines don't run into it. A line that
/// is all one `\boxed{…}` gets a box drawn around it.
fn place_display(out: &mut String, converted: &str) {
    const INDENT: &str = "\u{a0}\u{a0}"; // non-breaking spaces: Markdown would strip ordinary leading spaces
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str("  \n");
    }
    let mut lines = vec![];
    for line in converted.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let boxed = line.strip_prefix(BOX_OPEN).and_then(|l| l.strip_suffix(BOX_CLOSE)).filter(|l| !l.contains(BOX_OPEN));
        match boxed {
            Some(inner) => {
                let rule = "─".repeat(inner.width() + 2);
                lines.push(format!("┌{rule}┐"));
                lines.push(format!("│ {} │", escape(inner)));
                lines.push(format!("└{rule}┘"));
            }
            None => lines.push(emphasize_boxes(&escape(line))),
        }
    }
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("  \n");
        }
        out.push_str(INDENT);
        out.push_str(line);
    }
    out.push_str("  ");
}

/// `\boxed{…}` that can't get a drawn box (inline, or part of a longer line) is shown in bold instead.
fn emphasize_boxes(markdown: &str) -> String {
    markdown.replace([BOX_OPEN, BOX_CLOSE], "**")
}

/// Backslash-escape what Markdown would read as formatting in converted math.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '#' | '|') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// One TeX math expression → Unicode text. Covers what models write for science and engineering answers; unknown
/// commands lose their backslash rather than disappear.
pub fn tex_to_unicode(tex: &str) -> String {
    let chars: Vec<char> = tex.chars().collect();
    let mut out = Converter { chars: &chars, pos: 0 }.run(usize::MAX);
    // Collapse the runs of spaces that spacing commands and line breaks leave behind, line by line.
    out = out.lines().map(|l| l.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n");
    out.trim().to_string()
}

struct Converter<'a> {
    chars: &'a [char],
    pos: usize,
}

impl Converter<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// Convert until the end, or until an unmatched `}` when inside a group.
    fn run(&mut self, depth: usize) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                '}' if depth != usize::MAX => {
                    self.pos += 1;
                    return out;
                }
                '{' => {
                    self.pos += 1;
                    out.push_str(&self.run(0));
                }
                '\\' => out.push_str(&self.command()),
                '^' | '_' => {
                    self.pos += 1;
                    let arg = self.argument();
                    out.push_str(&script(&arg, c == '^'));
                }
                '~' => {
                    self.pos += 1;
                    out.push(' ');
                }
                '&' => self.pos += 1, // alignment points in aligned equations
                '\n' | '\t' => {
                    self.pos += 1;
                    out.push(' ');
                }
                _ => {
                    self.pos += 1;
                    out.push(c);
                }
            }
        }
        out
    }

    /// The next argument: a `{group}`, a command, or one character.
    fn argument(&mut self) -> String {
        while self.peek() == Some(' ') {
            self.pos += 1;
        }
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                self.run(0)
            }
            Some('\\') => self.command(),
            Some(c) => {
                self.pos += 1;
                c.to_string()
            }
            None => String::new(),
        }
    }

    /// The raw text of the next `{group}`, unconverted (for `\text` and `\ce`).
    fn raw_group(&mut self) -> String {
        while self.peek() == Some(' ') {
            self.pos += 1;
        }
        if self.peek() != Some('{') {
            return self.argument();
        }
        self.pos += 1;
        let (mut depth, start) = (1, self.pos);
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return self.chars[start..self.pos - 1].iter().collect();
                    }
                }
                _ => {}
            }
        }
        self.chars[start..].iter().collect()
    }

    fn command(&mut self) -> String {
        self.pos += 1; // the backslash
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        if self.pos == start {
            // A one-character command: spacing, escaped symbols, `\\` line breaks.
            let Some(c) = self.peek() else { return String::new() };
            self.pos += 1;
            return match c {
                ',' | ';' | ':' | ' ' | '>' => " ".into(),
                '!' => String::new(),
                '\\' => "\n".into(),
                other => other.to_string(),
            };
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        match name.as_str() {
            "boxed" | "fbox" => format!("{BOX_OPEN}{}{BOX_CLOSE}", self.argument().trim()),
            "text" | "textrm" | "mathrm" | "textnormal" | "mathbf" | "textbf" | "mathit" | "textit" | "mathsf"
            | "mathtt" | "texttt" | "operatorname" | "mbox" | "boldsymbol" | "bm" | "mathcal" | "mathbb" => {
                let raw = self.raw_group();
                if name.starts_with("text") || name == "mbox" {
                    raw.replace('~', " ")
                } else {
                    tex_to_unicode(&raw)
                }
            }
            "ce" | "chemfig" => chem(&self.raw_group()),
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let (num, den) = (self.argument(), self.argument());
                format!("{}/{}", group(&num), group(&den))
            }
            "sqrt" => {
                let root = if self.peek() == Some('[') {
                    let end = self.chars[self.pos..].iter().position(|c| *c == ']').map_or(self.chars.len(), |i| self.pos + i);
                    let n: String = self.chars[self.pos + 1..end].iter().collect();
                    self.pos = (end + 1).min(self.chars.len());
                    match n.trim() {
                        "3" => "∛",
                        "4" => "∜",
                        _ => "√",
                    }
                } else {
                    "√"
                };
                format!("{root}{}", group(&self.argument()))
            }
            "overline" | "bar" => format!("{}\u{305}", self.argument()),
            "hat" => format!("{}\u{302}", self.argument()),
            "vec" => format!("{}\u{20d7}", self.argument()),
            "dot" => format!("{}\u{307}", self.argument()),
            "tilde" => format!("{}\u{303}", self.argument()),
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl" | "Bigr" => {
                // The delimiter that follows is kept; `\left.` hides it.
                if self.peek() == Some('.') {
                    self.pos += 1;
                }
                String::new()
            }
            "displaystyle" | "textstyle" | "limits" | "nolimits" | "begin" | "end" => {
                if matches!(name.as_str(), "begin" | "end") {
                    self.raw_group(); // environment names (aligned, array…) aren't shown
                }
                String::new()
            }
            // Function names stay as words, with a space before their argument: `\sin\theta` → `sin θ`.
            other if FUNCTIONS.contains(&other) => format!("{other} "),
            other => {
                let text = symbol(other).map_or_else(|| other.to_string(), str::to_string);
                // TeX ignores the space after a command name, so `\Delta S` is ΔS. Keep it after operators and
                // arrows, where it reads better: `a \to b` stays a → b.
                if text.chars().all(char::is_alphabetic) {
                    while self.peek() == Some(' ') {
                        self.pos += 1;
                    }
                }
                text
            }
        }
    }
}

/// Parenthesize a fraction part that isn't a single term.
fn group(part: &str) -> String {
    let part = part.trim();
    if part.chars().count() > 1 && part.contains([' ', '+', '-', '−', '/', '·', '×']) {
        format!("({part})")
    } else {
        part.to_string()
    }
}

/// Superscript or subscript `text`, with Unicode script characters when every character has one.
fn script(text: &str, sup: bool) -> String {
    let text = text.trim();
    if sup && matches!(text, "°" | "\\circ" | "∘") {
        return "°".into();
    }
    if sup && matches!(text, "′" | "\\prime" | "'") {
        return "′".into();
    }
    let mapped: Option<String> = text.chars().map(|c| if sup { superscript(c) } else { subscript(c) }).collect();
    match mapped {
        Some(s) if !s.is_empty() => s,
        // No Unicode form: `K_eq`, and parentheses only around more than one term, `x^(n+1)`.
        _ if text.chars().all(char::is_alphanumeric) => format!("{}{text}", if sup { '^' } else { '_' }),
        _ => format!("{}({text})", if sup { '^' } else { '_' }),
    }
}

fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰', '1' => '¹', '2' => '²', '3' => '³', '4' => '⁴', '5' => '⁵', '6' => '⁶', '7' => '⁷', '8' => '⁸',
        '9' => '⁹', '+' => '⁺', '-' | '−' => '⁻', '=' => '⁼', '(' => '⁽', ')' => '⁾', 'n' => 'ⁿ', 'i' => 'ⁱ',
        'a' => 'ᵃ', 'b' => 'ᵇ', 'c' => 'ᶜ', 'd' => 'ᵈ', 'e' => 'ᵉ', 'f' => 'ᶠ', 'g' => 'ᵍ', 'h' => 'ʰ', 'j' => 'ʲ',
        'k' => 'ᵏ', 'l' => 'ˡ', 'm' => 'ᵐ', 'o' => 'ᵒ', 'p' => 'ᵖ', 'r' => 'ʳ', 's' => 'ˢ', 't' => 'ᵗ', 'u' => 'ᵘ',
        'v' => 'ᵛ', 'w' => 'ʷ', 'x' => 'ˣ', 'y' => 'ʸ', 'z' => 'ᶻ', 'T' => 'ᵀ', '°' => '°', ' ' => ' ',
        _ => return None,
    })
}

fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀', '1' => '₁', '2' => '₂', '3' => '₃', '4' => '₄', '5' => '₅', '6' => '₆', '7' => '₇', '8' => '₈',
        '9' => '₉', '+' => '₊', '-' | '−' => '₋', '=' => '₌', '(' => '₍', ')' => '₎', 'a' => 'ₐ', 'e' => 'ₑ',
        'h' => 'ₕ', 'i' => 'ᵢ', 'j' => 'ⱼ', 'k' => 'ₖ', 'l' => 'ₗ', 'm' => 'ₘ', 'n' => 'ₙ', 'o' => 'ₒ', 'p' => 'ₚ',
        'r' => 'ᵣ', 's' => 'ₛ', 't' => 'ₜ', 'u' => 'ᵤ', 'v' => 'ᵥ', 'x' => 'ₓ',
        _ => return None,
    })
}

/// mhchem's `\ce{…}`: counts after an element or bracket become subscripts, charges after `^` superscripts, and the
/// arrows become arrows. `\ce{2H2O -> 2H2 + O2}` → `2H₂O → 2H₂ + O₂`.
fn chem(formula: &str) -> String {
    let formula = formula.replace("<=>", "⇌").replace("<->", "↔").replace("->", "→").replace("<-", "←");
    let chars: Vec<char> = formula.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '^' {
            // A charge: ^2+, ^{3+}, ^-
            i += 1;
            let braced = chars.get(i) == Some(&'{');
            if braced {
                i += 1;
            }
            let start = i;
            while i < chars.len() && (if braced { chars[i] != '}' } else { chars[i].is_ascii_digit() || matches!(chars[i], '+' | '-') }) {
                i += 1;
            }
            let charge: String = chars[start..i].iter().collect();
            if braced {
                i += 1;
            }
            out.push_str(&script(&charge, true));
            continue;
        }
        if c == '_' {
            i += 1;
            continue;
        }
        let after_species = out.chars().last().is_some_and(|p| p.is_ascii_alphabetic() || matches!(p, ')' | ']') || subscript_digit(p));
        if c.is_ascii_digit() && after_species {
            out.push(subscript(c).unwrap_or(c));
        } else if c != '{' && c != '}' {
            out.push(c);
        }
        i += 1;
    }
    out
}

fn subscript_digit(c: char) -> bool {
    ('₀'..='₉').contains(&c)
}

const FUNCTIONS: &[&str] = &[
    "sin", "cos", "tan", "sec", "csc", "cot", "arcsin", "arccos", "arctan", "sinh", "cosh", "tanh", "log", "ln", "lg",
    "exp", "lim", "max", "min", "sup", "inf", "det", "gcd", "mod", "arg", "dim", "ker", "Pr",
];

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek
        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ", "epsilon" => "ε", "varepsilon" => "ε",
        "zeta" => "ζ", "eta" => "η", "theta" => "θ", "vartheta" => "ϑ", "iota" => "ι", "kappa" => "κ",
        "lambda" => "λ", "mu" => "μ", "nu" => "ν", "xi" => "ξ", "pi" => "π", "varpi" => "ϖ", "rho" => "ρ",
        "varrho" => "ϱ", "sigma" => "σ", "varsigma" => "ς", "tau" => "τ", "upsilon" => "υ", "phi" => "φ",
        "varphi" => "φ", "chi" => "χ", "psi" => "ψ", "omega" => "ω", "Gamma" => "Γ", "Delta" => "Δ",
        "Theta" => "Θ", "Lambda" => "Λ", "Xi" => "Ξ", "Pi" => "Π", "Sigma" => "Σ", "Upsilon" => "Υ", "Phi" => "Φ",
        "Psi" => "Ψ", "Omega" => "Ω",
        // Operators and relations
        "cdot" => "·", "times" => "×", "div" => "÷", "pm" => "±", "mp" => "∓", "ast" => "∗", "star" => "⋆",
        "circ" => "∘", "bullet" => "•", "le" | "leq" => "≤", "ge" | "geq" => "≥", "ne" | "neq" => "≠",
        "approx" => "≈", "sim" => "∼", "simeq" => "≃", "cong" => "≅", "equiv" => "≡", "propto" => "∝",
        "ll" => "≪", "gg" => "≫", "in" => "∈", "notin" => "∉", "subset" => "⊂", "subseteq" => "⊆",
        "supset" => "⊃", "cup" => "∪", "cap" => "∩", "emptyset" | "varnothing" => "∅", "forall" => "∀",
        "exists" => "∃", "neg" | "lnot" => "¬", "land" | "wedge" => "∧", "lor" | "vee" => "∨", "oplus" => "⊕",
        "otimes" => "⊗", "perp" => "⊥", "parallel" => "∥", "angle" => "∠", "therefore" => "∴", "because" => "∵",
        "sum" => "∑", "prod" => "∏", "int" => "∫", "iint" => "∬", "oint" => "∮", "partial" => "∂", "nabla" => "∇",
        "infty" => "∞", "hbar" => "ℏ", "ell" => "ℓ", "Re" => "ℜ", "Im" => "ℑ", "aleph" => "ℵ", "prime" => "′",
        "degree" => "°", "AA" | "angstrom" => "Å", "deg" => "°",
        // Arrows
        "to" | "rightarrow" | "longrightarrow" => "→", "leftarrow" | "gets" | "longleftarrow" => "←",
        "leftrightarrow" | "longleftrightarrow" => "↔", "Rightarrow" | "Longrightarrow" | "implies" => "⇒",
        "Leftarrow" | "Longleftarrow" => "⇐", "Leftrightarrow" | "Longleftrightarrow" | "iff" => "⇔",
        "rightleftharpoons" | "leftrightharpoons" | "rightleftarrows" => "⇌", "uparrow" => "↑",
        "downarrow" => "↓", "mapsto" => "↦",
        // Dots, spacing, delimiters and escapes
        "ldots" | "dots" | "cdots" | "dotsc" | "dotsb" => "…", "vdots" => "⋮", "ddots" => "⋱",
        "quad" => "  ", "qquad" => "   ", "enspace" | "thinspace" | "space" => " ", "langle" => "⟨",
        "rangle" => "⟩", "lfloor" => "⌊", "rfloor" => "⌋", "lceil" => "⌈", "rceil" => "⌉", "vert" | "mid" => "|",
        "Vert" => "‖", "lbrace" => "{", "rbrace" => "}", "backslash" => "\\",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_common_tex() {
        let t = tex_to_unicode;
        assert_eq!(t(r"\Delta S^\circ = \sum\nu_i\,S^\circ_{\text{products}}"), "ΔS° = ∑νᵢ S°_products");
        assert_eq!(t(r"\mathrm{CH_4(g)} + 2\mathrm{O_2(g)} \rightarrow \mathrm{CO_2(g)}"), "CH₄(g) + 2O₂(g) → CO₂(g)");
        assert_eq!(t(r"-242\ \text{J mol}^{-1}\text{K}^{-1}"), "-242 J mol⁻¹K⁻¹");
        assert_eq!(t(r"\text{Cu}^{2+} + 2e^{-}"), "Cu²⁺ + 2e⁻");
        assert_eq!(t(r"\frac{a+b}{2} \le \sqrt{x^2+1}"), "(a+b)/2 ≤ √(x²+1)");
        assert_eq!(t(r"\frac{1}{2}mv^2"), "1/2mv²");
        assert_eq!(t(r"x^{n+1} + y_{i,j}"), "xⁿ⁺¹ + y_(i,j)");
        assert_eq!(t(r"E = mc^2 \quad \text{and} \quad K_{eq}"), "E = mc² and K_eq");
        assert_eq!(t(r"\ce{2H2O -> 2H2 + O2}"), "2H₂O → 2H₂ + O₂");
        assert_eq!(t(r"\ce{Fe^{3+} + e- <=> Fe^{2+}}"), "Fe³⁺ + e- ⇌ Fe²⁺");
        assert_eq!(t(r"\left( x \right) \cdot \unknown"), "( x ) · unknown");
        assert_eq!(t(r"a &= b \\ &= c"), "a = b\n= c");
    }

    #[test]
    fn finds_math_in_markdown_and_skips_code() {
        let md = "ΔS for the reaction:\n\n\\[\n\\Delta S = 1 \\\\\n= 2\n\\]\n\nwhere \\(\\nu_i\\) are coefficients.";
        assert_eq!(render_math(md), "ΔS for the reaction:\n\n\u{a0}\u{a0}ΔS = 1  \n\u{a0}\u{a0}= 2  \n\nwhere νᵢ are coefficients.");
        // Two display blocks on consecutive lines stay on separate lines.
        let two = "\\[\\text{Cu} \\to \\text{Cu}^{2+}\\]\n\\[\\text{Fe}^{3+} \\to \\text{Fe}^{2+}\\]\n";
        assert_eq!(render_math(two), "\u{a0}\u{a0}Cu → Cu²⁺  \n\u{a0}\u{a0}Fe³⁺ → Fe²⁺  \n");
        // Code is untouched, and so are prices and escaped dollars.
        let code = "Run `echo \\(x\\)` and\n```tex\n\\[ x^2 \\]\n```\n";
        assert_eq!(render_math(code), code);
        assert_eq!(render_math("It costs $5 and $10, or US$3."), "It costs $5 and $10, or US$3.");
        assert_eq!(render_math("inline $x^2$ and \\$5"), "inline x² and \\$5");
        // Unclosed math (a reply still streaming) is left alone.
        assert_eq!(render_math("so \\[ \\Delta"), "so \\[ \\Delta");
        // \boxed: a drawn box when it's a whole display line, bold otherwise.
        let boxed = "Total:\n\\[ \\boxed{x = 2\\times 3} \\]\nso $\\boxed{42}$ and $$y = \\boxed{7}$$";
        let drawn = "Total:\n\u{a0}\u{a0}┌──────────┐  \n\u{a0}\u{a0}│ x = 2× 3 │  \n\u{a0}\u{a0}└──────────┘  \nso **42** and   \n\u{a0}\u{a0}y = **7**  ";
        assert_eq!(render_math(boxed), drawn);
        // Converted text is escaped for Markdown: subscripts can't become italics.
        assert_eq!(render_math("\\(a_{xy} * b_{xy}\\)"), "a\\_xy \\* b\\_xy");
    }
}
