//! A virtual terminal model.
//!
//! Nothing about recording or replay depends on this module. It exists so that
//! the non-terminal views of a session — inspection, HTML export, searching
//! visible text — have real structure to work with instead of re-guessing what
//! the byte stream meant.

pub mod attrs;
pub mod grid;
pub mod parser;

pub use attrs::{Attrs, Cell, Color};
pub use grid::{char_width, Cursor, Screen, ShellMark};
pub use parser::{Action, Params, Parser};
