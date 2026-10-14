//! Instance-bound mutation capabilities.
//!
//! A permit owns a short exclusive borrow of the checked writer. Guarded
//! operations are methods on that permit: there is no second writer argument
//! on which a witness issued by writer A could be spent. Moving/replacing the
//! original writer variable while its permit remains live is rejected by the borrow
//! checker. The operation also checks the current flag, so a permission cannot
//! hide a poison raised by an earlier step of the same operation.
//!
//! This uses the whole writer borrow rather than splitting every writer's
//! public representation. It adds no allocation, atomics, locks or syscalls.
//! The handle carries a writer reference and a static error context. PoisonFlag
//! remains two booleans. Private constructors and source gates keep permits
//! tied to each writer's authoritative flag.
use crate::{Error, Result};
use core::{
    fmt,
    ops::{Deref, DerefMut},
};

pub trait GuardedWriter: Sized {
    fn poison_flag(&self) -> &PoisonFlag;
    fn poison_flag_mut(&mut self) -> &mut PoisonFlag;
    fn check_mutation(&self, context: &'static str) -> Result<()> {
        if self.poison_flag().is_refusing() {
            Err(Error::WriterPoisoned(context))
        } else {
            Ok(())
        }
    }
    fn writer_permit(&mut self, context: &'static str) -> Result<MutationPermit<'_, Self>> {
        PoisonFlag::issue(self, context)
    }
}

/// Exclusive access to the writer whose flag was checked.
#[must_use = "drop the handle only if its mutation is abandoned"]
pub struct MutationPermit<'a, W: ?Sized> {
    writer: &'a mut W,
    context: &'static str,
}
impl<W: ?Sized> fmt::Debug for MutationPermit<'_, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutationPermit")
    }
}
impl<W: ?Sized> Deref for MutationPermit<'_, W> {
    type Target = W;
    fn deref(&self) -> &W {
        self.writer
    }
}
impl<W: ?Sized> DerefMut for MutationPermit<'_, W> {
    fn deref_mut(&mut self) -> &mut W {
        self.writer
    }
}
impl<'a, W: GuardedWriter> MutationPermit<'a, W> {
    pub(crate) fn check(&self) -> Result<()> {
        self.writer.check_mutation(self.context)
    }
    /// Open an in-flight window on exactly the borrowed writer.
    pub fn begin_mutation(self) -> Result<MutationInFlight<'a, W>> {
        self.check()?;
        self.writer.poison_flag_mut().in_flight = true;
        Ok(MutationInFlight {
            writer: self.writer,
        })
    }
}

/// Exclusive access during an authorized reversible mutation window.
#[must_use = "an unclosed window leaves its writer refusing later mutations"]
pub struct MutationInFlight<'a, W: ?Sized> {
    writer: &'a mut W,
}
impl<W: ?Sized> fmt::Debug for MutationInFlight<'_, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutationInFlight")
    }
}
impl<W: ?Sized> Deref for MutationInFlight<'_, W> {
    type Target = W;
    fn deref(&self) -> &W {
        self.writer
    }
}
impl<W: ?Sized> DerefMut for MutationInFlight<'_, W> {
    fn deref_mut(&mut self) -> &mut W {
        self.writer
    }
}
impl<'a, W: GuardedWriter> MutationInFlight<'a, W> {
    /// Close this writer's window; one-way poison is preserved.
    pub fn end_mutation(self) -> &'a mut W {
        self.writer.poison_flag_mut().in_flight = false;
        self.writer
    }
}

#[derive(Debug)]
pub struct PoisonFlag {
    poisoned: bool,
    in_flight: bool,
}
impl PoisonFlag {
    /// Non-const deliberately: a static decoy must not replace an owner's flag.
    pub fn healthy() -> Self {
        Self {
            poisoned: false,
            in_flight: false,
        }
    }
    // The only permit constructor; no bare flag can issue a witness.
    fn issue<'a, W: GuardedWriter>(
        writer: &'a mut W,
        context: &'static str,
    ) -> Result<MutationPermit<'a, W>> {
        writer.check_mutation(context)?;
        Ok(MutationPermit { writer, context })
    }
    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
    }
    pub(crate) fn is_refusing(&self) -> bool {
        self.poisoned || self.in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Writer {
        poison: PoisonFlag,
        value: u64,
    }
    impl GuardedWriter for Writer {
        fn poison_flag(&self) -> &PoisonFlag {
            &self.poison
        }
        fn poison_flag_mut(&mut self) -> &mut PoisonFlag {
            &mut self.poison
        }
    }
    impl Writer {
        fn healthy() -> Self {
            Self {
                poison: PoisonFlag::healthy(),
                value: 0,
            }
        }
    }
    #[test]
    fn a_poisoned_flag_issues_no_permit() {
        let mut writer = Writer::healthy();
        assert!(writer.writer_permit("test").is_ok());
        writer.poison.poison();
        assert!(matches!(
            writer.writer_permit("test"),
            Err(Error::WriterPoisoned("test"))
        ));
    }
    #[test]
    fn an_open_in_flight_window_refuses_a_second_permit() {
        let mut writer = Writer::healthy();
        let mut window = writer
            .writer_permit("test")
            .unwrap()
            .begin_mutation()
            .unwrap();
        assert!(window.writer_permit("test").is_err());
        let writer = window.end_mutation();
        assert!(writer.writer_permit("test").is_ok());
    }
    #[test]
    fn ending_a_window_does_not_clear_a_poison_raised_inside_it() {
        let mut writer = Writer::healthy();
        let mut window = writer
            .writer_permit("test")
            .unwrap()
            .begin_mutation()
            .unwrap();
        window.poison.poison();
        let writer = window.end_mutation();
        assert!(writer.writer_permit("test").is_err());
    }
    #[test]
    fn same_type_writers_have_independent_bound_handles() {
        let mut a = Writer::healthy();
        let mut b = Writer::healthy();
        b.poison.poison();
        let mut permit = a.writer_permit("a").unwrap();
        permit.check().unwrap();
        permit.value = 19;
        assert!(b.writer_permit("b").is_err());
        assert_eq!(b.value, 0);
        drop(permit);
        assert_eq!(a.value, 19);
    }
    #[test]
    fn replacing_contents_through_a_handle_cannot_launder_a_poisoned_writer() {
        let mut a = Writer::healthy();
        let mut b = Writer::healthy();
        b.poison.poison();
        let mut permit = a.writer_permit("checked a").unwrap();
        // Mutable access can move contents even though the original variable
        // is borrowed. The guarded step must check the current owner state.
        std::mem::swap(&mut *permit, &mut b);
        assert!(permit.check().is_err());
        assert!(permit.begin_mutation().is_err());
        assert!(b.writer_permit("healthy old a").is_ok());
    }

    #[test]
    fn a_live_handle_rechecks_poison_before_another_guarded_step() {
        let mut writer = Writer::healthy();
        let mut permit = writer.writer_permit("test").unwrap();
        permit.poison.poison();
        assert!(permit.check().is_err());
        assert!(permit.begin_mutation().is_err());
    }
}
