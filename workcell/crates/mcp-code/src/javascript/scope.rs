//! What each name the snippet declares is bound to, kept only to refuse what Python would read
//! differently: a declaration that shadows another or takes a name the translation needs, a name
//! used where nothing declared it, and an assignment Python would make local to a function.

use std::collections::HashMap;

use super::expressions::Fact;
use super::library;
use super::python::{Builtin, Import, Name};

/// The global `throw new Error()` reads, which the snippet may not redeclare.
pub(super) const ERROR: &str = "Error";

#[derive(Clone)]
pub(super) struct Binding {
    pub(super) kind: BindingKind,
    /// What its value is known to be: only a `const` keeps more than an optional value's taint.
    pub(super) fact: Fact,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BindingKind {
    Const,
    Let,
    Parameter,
    Function { is_async: bool },
}

/// What a function-like scope lets its body do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Frame {
    Module,
    Function {
        is_async: bool,
    },
    /// A lifted `filter`, `find`, `some` or `every` callback, whose every return is a test.
    Predicate,
    /// An expression-bodied callback that became a comprehension: nothing can be lifted out of it.
    Comprehension,
}

#[derive(Default)]
pub(super) struct Scopes {
    functions: Vec<Function>,
}

struct Function {
    frame: Frame,
    blocks: Vec<HashMap<String, Binding>>,
    /// Loops around what is lowered now, inside this function and outside any `forEach` body.
    loops: usize,
    /// `forEach` bodies around what is lowered now, where `return` would leave the callback.
    callbacks: usize,
}

impl Scopes {
    pub(super) fn enter(&mut self, frame: Frame) {
        self.functions.push(Function {
            frame,
            blocks: vec![HashMap::new()],
            loops: 0,
            callbacks: 0,
        });
    }

    pub(super) fn leave(&mut self) {
        self.functions.pop();
    }

    pub(super) fn open(&mut self) {
        if let Some(function) = self.functions.last_mut() {
            function.blocks.push(HashMap::new());
        }
    }

    pub(super) fn close(&mut self) {
        if let Some(function) = self.functions.last_mut() {
            function.blocks.pop();
        }
    }

    /// Binds `written` in the innermost block. `None` when Python could not keep it apart: it is
    /// no identifier, it names something the translation or the host needs, or it would shadow.
    pub(super) fn declare(
        &mut self,
        written: &str,
        binding: Binding,
        hosts: impl Fn(&str) -> bool,
    ) -> Option<Name> {
        let name = Name::parse(written)?;
        let taken = written == ERROR
            || library::is_global_object(written)
            || library::global(written).is_some()
            || Builtin::ALL.iter().any(|builtin| builtin.name() == written)
            || Import::ALL.iter().any(|import| import.name() == written)
            || hosts(written)
            || self.resolve(written).is_some();
        if taken {
            return None;
        }
        self.functions
            .last_mut()?
            .blocks
            .last_mut()?
            .insert(written.to_owned(), binding);
        Some(name)
    }

    pub(super) fn resolve(&self, written: &str) -> Option<&Binding> {
        self.functions
            .iter()
            .rev()
            .flat_map(|function| function.blocks.iter().rev())
            .find_map(|block| block.get(written))
    }

    /// Whether assigning `written` here rebinds the variable JavaScript would: a `let` or a
    /// parameter of the function lowered now. A `const` or a name from an outer function refuses.
    pub(super) fn assignable(&self, written: &str) -> bool {
        self.functions.last().is_some_and(|function| {
            function
                .blocks
                .iter()
                .rev()
                .find_map(|block| block.get(written))
                .is_some_and(|binding| {
                    matches!(binding.kind, BindingKind::Let | BindingKind::Parameter)
                })
        })
    }

    /// Drops what a `let` was known to be once it is reassigned.
    pub(super) fn forget(&mut self, written: &str, fact: Fact) {
        if let Some(binding) = self.functions.last_mut().and_then(|function| {
            function
                .blocks
                .iter_mut()
                .rev()
                .find_map(|block| block.get_mut(written))
        }) {
            binding.fact = fact;
        }
    }

    pub(super) fn frame(&self) -> Frame {
        self.functions
            .last()
            .map_or(Frame::Module, |function| function.frame)
    }

    /// Whether `await` is allowed here: in the module or an async function, outside any
    /// `forEach` body.
    pub(super) fn awaits(&self) -> bool {
        self.functions.last().is_some_and(|function| {
            function.callbacks == 0
                && matches!(
                    function.frame,
                    Frame::Module | Frame::Function { is_async: true }
                )
        })
    }

    pub(super) fn in_loop(&self) -> bool {
        self.functions
            .last()
            .is_some_and(|function| function.loops > 0)
    }

    pub(super) fn in_callback(&self) -> bool {
        self.functions
            .last()
            .is_some_and(|function| function.callbacks > 0)
    }

    pub(super) fn enter_loop(&mut self) {
        if let Some(function) = self.functions.last_mut() {
            function.loops += 1;
        }
    }

    pub(super) fn leave_loop(&mut self) {
        if let Some(function) = self.functions.last_mut() {
            function.loops -= 1;
        }
    }

    /// Enters a `forEach` body, where no enclosing loop is in reach of `break` or `continue`.
    /// Returns the loops it hid, for [`Self::leave_callback`].
    pub(super) fn enter_callback(&mut self) -> usize {
        self.functions.last_mut().map_or(0, |function| {
            function.callbacks += 1;
            std::mem::take(&mut function.loops)
        })
    }

    pub(super) fn leave_callback(&mut self, loops: usize) {
        if let Some(function) = self.functions.last_mut() {
            function.callbacks -= 1;
            function.loops = loops;
        }
    }
}
