//! greeter: the login screen. init runs it on the console in place of a
//! `login:` prompt.
//!
//! A box in the middle of the screen asks for a name and a password, and
//! has a third field, the session: Up and Down (or Tab) move between the
//! fields, Left and Right pick the text console or the desktop (the
//! default, until someone picks the console), Enter logs
//! in. It's drawn with escape sequences, so it works on the framebuffer
//! console and over a serial line alike, and needs nothing graphical: if
//! the desktop is broken, Text still gets you a shell.
//!
//! It remembers who logged in last and each person's session, in
//! /data/etc/greeter on an installed system (in /run on the live one,
//! which forgets everything at shutdown anyway).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eli::io::{self, STDOUT};
use eli::users::{self, User};
use eli::{fs, process, system, term, time};

use crate::editor::{Key, read_key};

const DESKTOP: &str = "/bin/desktop";
const DATA: &str = "/data";
/// `nobody`'s user id: an account, but not a person.
const NOBODY: u32 = 65534;
const BOX_WIDTH: usize = 46;
const FIELD_WIDTH: usize = 28;
const MAX_NAME: usize = 32;
const MAX_PASSWORD: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Text,
    Desktop,
}

impl Session {
    fn name(self) -> &'static str {
        match self {
            Session::Text => "Text",
            Session::Desktop => "Desktop",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Session::Text => "text",
            Session::Desktop => "desktop",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        match word {
            "text" => Some(Session::Text),
            "desktop" => Some(Session::Desktop),
            _ => None,
        }
    }

    /// The other one (there are only two).
    fn next(self) -> Self {
        match self {
            Session::Text if fs::exists(DESKTOP) => Session::Desktop,
            _ => Session::Text,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Session,
    Name,
    Password,
}

impl Field {
    fn below(self) -> Self {
        match self {
            Field::Session => Field::Name,
            Field::Name => Field::Password,
            Field::Password => Field::Session,
        }
    }

    fn above(self) -> Self {
        match self {
            Field::Session => Field::Password,
            Field::Name => Field::Session,
            Field::Password => Field::Name,
        }
    }
}

/// What's remembered between logins.
struct Memory {
    last_user: Option<String>,
    sessions: Vec<(String, Session)>,
}

impl Memory {
    fn path() -> &'static str {
        if fs::mounts().iter().any(|mount| mount.target == DATA) { "/data/etc/greeter" } else { "/run/greeter" }
    }

    fn load() -> Self {
        let mut memory = Memory { last_user: None, sessions: Vec::new() };
        let text = fs::read_to_string(Self::path()).unwrap_or_default();
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            match words[..] {
                ["last", name] => memory.last_user = Some(name.into()),
                ["session", name, session] => {
                    if let Some(session) = Session::from_word(session) {
                        memory.sessions.push((name.into(), session));
                    }
                }
                _ => {}
            }
        }
        memory
    }

    fn session_of(&self, name: &str) -> Option<Session> {
        self.sessions.iter().find(|(user, _)| user == name).map(|&(_, session)| session)
    }

    fn remember(&mut self, name: &str, session: Session) {
        self.last_user = Some(name.into());
        self.sessions.retain(|(user, _)| user != name);
        self.sessions.push((name.into(), session));
        let mut text = format!("last {name}\n");
        for (user, session) in &self.sessions {
            text.push_str(&format!("session {user} {}\n", session.word()));
        }
        let _ = fs::write(Self::path(), text.as_bytes());
        let _ = fs::set_permissions(Self::path(), 0o600);
    }
}

struct Screen {
    field: Field,
    session: Session,
    name: String,
    password: String,
    message: String,
    /// The name whose remembered session `session` shows.
    session_for: String,
}

/// `greeter`: the login screen (see the module documentation).
pub fn greeter(_arguments: &[String]) -> i32 {
    if users::uid() != 0 {
        eli::eprintln!("greeter: only root can start a login session");
        return 1;
    }
    let Ok(original) = term::enter_raw_mode(io::STDIN) else {
        // Not a terminal: plain `login` copes with that.
        return crate::accounts::login(&["login".into()]);
    };

    let mut memory = Memory::load();
    // Whoever logged in last, or else the first ordinary account (on a
    // fresh system, that's the only person who'll use it).
    let name = memory
        .last_user
        .clone()
        .filter(|name| users::user_named(name).is_some())
        .or_else(|| {
            users::users()
                .into_iter()
                .find(|user| (users::FIRST_USER_ID..NOBODY).contains(&user.uid))
                .map(|user| user.name)
        })
        .unwrap_or_default();
    let mut screen = Screen {
        field: if name.is_empty() { Field::Name } else { Field::Password },
        session: Session::Text,
        name,
        password: String::new(),
        message: String::new(),
        session_for: String::new(),
    };
    screen.follow_name(&memory);

    loop {
        screen.draw();
        let Some(key) = read_key() else {
            let _ = term::set_settings(io::STDIN, &original);
            return 1;
        };
        screen.message.clear();
        match key {
            Key::Up => screen.field = screen.field.above(),
            Key::Down | Key::Tab => screen.field = screen.field.below(),
            Key::Left | Key::Right if screen.field == Field::Session => screen.session = screen.session.next(),
            Key::Char(' ') if screen.field == Field::Session => screen.session = screen.session.next(),
            Key::Char(c) => match screen.field {
                Field::Name if screen.name.len() < MAX_NAME && !c.is_whitespace() && c != ':' => screen.name.push(c),
                Field::Password if screen.password.len() < MAX_PASSWORD => screen.password.push(c),
                _ => {}
            },
            Key::Backspace => {
                match screen.field {
                    Field::Name => screen.name.pop(),
                    Field::Password => screen.password.pop(),
                    Field::Session => None,
                };
            }
            Key::Control(b'u') => match screen.field {
                Field::Name => screen.name.clear(),
                Field::Password => screen.password.clear(),
                Field::Session => {}
            },
            Key::Function(1) => {
                screen.message = format!("power off: {}", process::power_off());
            }
            Key::Function(2) => {
                screen.message = format!("reboot: {}", process::reboot());
            }
            Key::Enter => match screen.field {
                Field::Session => screen.field = Field::Name,
                Field::Name if screen.name.is_empty() => screen.message = "Type your user name".into(),
                Field::Name => screen.field = Field::Password,
                Field::Password if screen.name.is_empty() => screen.field = Field::Name,
                Field::Password => {
                    if let Some(user) = screen.authenticate() {
                        memory.remember(&user.name, screen.session);
                        let _ = term::set_settings(io::STDIN, &original);
                        output("\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
                        let program = match screen.session {
                            Session::Desktop => Some(DESKTOP),
                            Session::Text => None,
                        };
                        return crate::accounts::start_session(&user, program);
                    }
                    screen.password.clear();
                    screen.message = "Wrong user name or password".into();
                }
            },
            _ => {}
        }
        screen.follow_name(&memory);
    }
}

fn output(text: &str) {
    let _ = io::write_all(STDOUT, text.as_bytes());
}

impl Screen {
    /// Shows the session the person named last used, once they've been
    /// named (but leaves it alone once they've chosen one for this login).
    fn follow_name(&mut self, memory: &Memory) {
        if self.field == Field::Name || self.name == self.session_for {
            return;
        }
        self.session_for = self.name.clone();
        // The desktop, unless they chose the console last time (or the
        // desktop isn't there).
        self.session = match memory.session_of(&self.name) {
            Some(Session::Text) => Session::Text,
            _ if fs::exists(DESKTOP) => Session::Desktop,
            _ => Session::Text,
        };
    }

    /// The account, if the password is right for it. Unknown names and
    /// wrong passwords take the same time and get the same answer, so
    /// guessing names gains nothing.
    fn authenticate(&mut self) -> Option<User> {
        let account = users::user_named(&self.name);
        let right = account.is_some()
            && (users::has_empty_password(&self.name) || users::check_password(&self.name, &self.password));
        if right {
            return account;
        }
        self.message = "Checking...".into();
        self.draw();
        time::sleep_ms(2000);
        None
    }

    fn draw(&self) {
        let (columns, rows) = term::size(io::STDIN);
        let (columns, rows) = (usize::from(columns), usize::from(rows));
        let height = 11;
        let left = columns.saturating_sub(BOX_WIDTH) / 2 + 1;
        let top = rows.saturating_sub(height) / 2 + 1;
        let inner = BOX_WIDTH - 2;
        let mut out = String::from("\x1b[?25l\x1b[0m\x1b[2J");
        let mut put = |row: usize, text: &str| out.push_str(&format!("\x1b[{};{left}H{text}", top + row));

        let horizontal = "─".repeat(inner);
        put(0, &format!("\x1b[36m┌{horizontal}┐\x1b[0m"));
        for row in 1..height - 1 {
            put(row, &format!("\x1b[36m│\x1b[0m{}\x1b[36m│\x1b[0m", " ".repeat(inner)));
        }
        put(height - 1, &format!("\x1b[36m└{horizontal}┘\x1b[0m"));

        let title = "eliOS";
        put(2, &format!("\x1b[{}C\x1b[1;97m{title}\x1b[0m", 1 + (inner - title.len()) / 2));

        let label = |field: Field, text: &str| {
            if self.field == field { format!("\x1b[1;36m{text:<10}\x1b[0m") } else { format!("{text:<10}") }
        };
        let value = |field: Field, text: &str| {
            let shown: String =
                text.chars().rev().take(FIELD_WIDTH - 1).collect::<Vec<_>>().into_iter().rev().collect();
            let padding = " ".repeat(FIELD_WIDTH - shown.chars().count());
            let ink = if self.field == field { "\x1b[100;97m" } else { "\x1b[40m" };
            format!("{ink}{shown}{padding}\x1b[0m")
        };
        let session = format!("< {:^w$} >", self.session.name(), w = FIELD_WIDTH - 4);
        let session = if self.field == Field::Session {
            format!("\x1b[100;97m{session}\x1b[0m")
        } else {
            format!("\x1b[40m{session}\x1b[0m")
        };
        put(4, &format!("\x1b[3C{}{session}", label(Field::Session, "session")));
        put(5, &format!("\x1b[3C{}{}", label(Field::Name, "login"), value(Field::Name, &self.name)));
        let stars = "*".repeat(self.password.chars().count());
        put(6, &format!("\x1b[3C{}{}", label(Field::Password, "password"), value(Field::Password, &stars)));
        if !self.message.is_empty() {
            let message: String = self.message.chars().take(inner - 4).collect();
            put(8, &format!("\x1b[3C\x1b[33m{message}\x1b[0m"));
        }

        // Help along the bottom of the screen.
        let host = system::uname().hostname;
        out.push_str(&format!(
            "\x1b[{rows};2H\x1b[90mF1 power off   F2 reboot   Up/Down move   Left/Right session\x1b[0m"
        ));
        out.push_str(&format!("\x1b[{rows};{}H\x1b[90m{host}\x1b[0m", columns.saturating_sub(host.len())));

        // The cursor sits at the end of the field being typed in.
        let cursor = match self.field {
            Field::Session => None,
            Field::Name => Some((top + 5, self.name.chars().count())),
            Field::Password => Some((top + 6, self.password.chars().count())),
        };
        if let Some((row, length)) = cursor {
            let column = left + 3 + 10 + length.min(FIELD_WIDTH - 1);
            out.push_str(&format!("\x1b[{row};{column}H\x1b[?25h"));
        }
        output(&out);
    }
}
