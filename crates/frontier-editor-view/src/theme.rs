// SPDX-License-Identifier: Apache-2.0
//! Colours from the Frontier design tokens (the app's `main.css` custom properties).
//! Values are sRGB 0..1 with alpha; the renderer converts to linear for the sRGB
//! surface formats the viewers use.

use frontier_editor::decoration::Severity;
use frontier_editor::diff::HunkKind;
use frontier_editor::syntax::TokenKind;

pub type Rgba = [f32; 4];

pub const fn hex(rgb: u32) -> Rgba {
    [
        ((rgb >> 16) & 0xff) as f32 / 255.0,
        ((rgb >> 8) & 0xff) as f32 / 255.0,
        (rgb & 0xff) as f32 / 255.0,
        1.0,
    ]
}

pub const fn alpha(c: Rgba, a: f32) -> Rgba {
    [c[0], c[1], c[2], a]
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    pub bg_0: Rgba,
    pub bg_1: Rgba,
    pub bg_2: Rgba,
    pub bg_3: Rgba,
    pub line_1: Rgba,
    pub line_2: Rgba,
    pub fg_0: Rgba,
    pub fg_1: Rgba,
    pub fg_2: Rgba,
    pub fg_3: Rgba,
    pub accent: Rgba,
    pub accent_bg: Rgba,
    pub ok: Rgba,
    pub warn: Rgba,
    pub fail: Rgba,
    // syntax
    pub keyword: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub comment: Rgba,
    pub property: Rgba,
    pub type_: Rgba,
    pub function: Rgba,
    pub constant: Rgba,
    pub heading: Rgba,
    pub link: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            name: "dark",
            bg_0: hex(0x0a0c10),
            bg_1: hex(0x0f1218),
            bg_2: hex(0x151a22),
            bg_3: hex(0x1b2130),
            line_1: [1.0, 1.0, 1.0, 0.06],
            line_2: [1.0, 1.0, 1.0, 0.11],
            fg_0: hex(0xeef1f6),
            fg_1: hex(0xc5cbd6),
            fg_2: hex(0x8d95a5),
            fg_3: hex(0x5e6675),
            accent: hex(0x6ea0ff),
            accent_bg: [110.0 / 255.0, 160.0 / 255.0, 1.0, 0.14],
            ok: hex(0x3dd68c),
            warn: hex(0xf5b74e),
            fail: hex(0xff6b6b),
            keyword: hex(0xa78bfa),
            string: hex(0x3dd6a0),
            number: hex(0xf0a35e),
            comment: hex(0x5e6675),
            property: hex(0x6ea0ff),
            type_: hex(0x4fd1c5),
            function: hex(0x9db9ff),
            constant: hex(0xff8fab),
            heading: hex(0xeef1f6),
            link: hex(0x5eb7ff),
        }
    }

    pub fn light() -> Self {
        Self {
            name: "light",
            bg_0: hex(0xf3f5f8),
            bg_1: hex(0xffffff),
            bg_2: hex(0xffffff),
            bg_3: hex(0xeef1f5),
            line_1: [16.0 / 255.0, 24.0 / 255.0, 40.0 / 255.0, 0.07],
            line_2: [16.0 / 255.0, 24.0 / 255.0, 40.0 / 255.0, 0.13],
            fg_0: hex(0x0f1420),
            fg_1: hex(0x2d3545),
            fg_2: hex(0x5b6474),
            fg_3: hex(0x8a93a3),
            accent: hex(0x2f6fed),
            accent_bg: [47.0 / 255.0, 111.0 / 255.0, 237.0 / 255.0, 0.12],
            ok: hex(0x1f9d63),
            warn: hex(0xb97a12),
            fail: hex(0xd23f3f),
            keyword: hex(0x6d4fd6),
            string: hex(0x1f8a63),
            number: hex(0xb3611c),
            comment: hex(0x8a93a3),
            property: hex(0x2f6fed),
            type_: hex(0x1d8d84),
            function: hex(0x1d5be0),
            constant: hex(0xc93f6b),
            heading: hex(0x0f1420),
            link: hex(0x1a7fc9),
        }
    }

    pub fn by_name(name: &str) -> Self {
        if name == "light" {
            Self::light()
        } else {
            Self::dark()
        }
    }

    /// The same theme with another set of syntax colours: `vivid` strengthens them,
    /// `quiet` keeps keys and strings and puts the rest in the text colour.
    pub fn with_syntax(mut self, name: &str) -> Self {
        fn vivid(c: Rgba) -> Rgba {
            // push each channel away from the colour's own grey
            let grey = (c[0] + c[1] + c[2]) / 3.0;
            let f = |v: f32| (grey + (v - grey) * 1.45).clamp(0.0, 1.0);
            [f(c[0]), f(c[1]), f(c[2]), c[3]]
        }
        match name {
            "vivid" => {
                self.keyword = vivid(self.keyword);
                self.string = vivid(self.string);
                self.number = vivid(self.number);
                self.constant = vivid(self.constant);
            }
            "quiet" => {
                self.keyword = self.fg_1;
                self.number = self.fg_1;
                self.constant = self.fg_1;
            }
            _ => {}
        }
        self
    }

    pub fn token(&self, kind: TokenKind) -> Rgba {
        match kind {
            TokenKind::Keyword => self.keyword,
            TokenKind::String | TokenKind::Escape => self.string,
            TokenKind::Number => self.number,
            TokenKind::Comment => self.comment,
            TokenKind::Property => self.property,
            TokenKind::Type => self.type_,
            TokenKind::Function => self.function,
            TokenKind::Variable | TokenKind::Text => self.fg_1,
            TokenKind::Operator | TokenKind::Punctuation => self.fg_2,
            TokenKind::Constant => self.constant,
            TokenKind::Heading => self.heading,
            TokenKind::Emphasis => self.fg_0,
            TokenKind::Link => self.link,
            TokenKind::Error => self.fail,
            TokenKind::Warning => self.warn,
        }
    }

    pub fn severity(&self, s: Severity) -> Rgba {
        match s {
            Severity::Error => self.fail,
            Severity::Warning => self.warn,
            Severity::Info => self.accent,
            Severity::Hint => self.fg_3,
        }
    }

    pub fn hunk(&self, k: HunkKind) -> Rgba {
        match k {
            HunkKind::Insert => self.ok,
            HunkKind::Delete => self.fail,
            HunkKind::Modify => self.warn,
        }
    }

    /// Background for a row highlight class from `ViewLine::highlights`.
    pub fn highlight(&self, class: &str) -> Option<Rgba> {
        Some(match class {
            "selection" => alpha(self.accent, 0.28),
            "search" => alpha(self.warn, 0.30),
            "bracket" => alpha(self.accent, 0.40),
            "hunk_insert" => alpha(self.ok, 0.14),
            "hunk_delete" => alpha(self.fail, 0.14),
            "hunk_modify" => alpha(self.warn, 0.12),
            "diagnostic_error" => alpha(self.fail, 0.18),
            "diagnostic_warning" => alpha(self.warn, 0.16),
            "diagnostic_info" => alpha(self.accent, 0.12),
            "diagnostic_hint" => alpha(self.fg_3, 0.10),
            _ => return None,
        })
    }

    /// Minimap pixel colour for a token kind (dimmer than the text).
    pub fn minimap(&self, kind: Option<TokenKind>) -> Rgba {
        match kind {
            Some(k) => alpha(self.token(k), 0.75),
            None => alpha(self.fg_2, 0.45),
        }
    }
}
