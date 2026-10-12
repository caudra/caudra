use std::env;
use std::fs;

use crate::terminal;
use arboard::Clipboard;
#[cfg(target_os = "linux")]
use arboard::{LinuxClipboardKind, SetExtLinux};

const WSL_DISTRO_NAME: &str = "WSL_DISTRO_NAME";
const WSL_INTEROP: &str = "WSL_INTEROP";
/// `sudo` and `env -i` drop the WSL variables, but the kernel release still
/// names Microsoft: `-microsoft-standard-WSL2` on WSL 2, `-Microsoft` on WSL 1.
const OSRELEASE_PATH: &str = "/proc/sys/kernel/osrelease";
const WSL_KERNEL_MARKER: &str = "microsoft";

pub(crate) enum CopyResult {
    Copied,
    Noop,
}

pub(crate) struct ClipboardState {
    native: Option<Clipboard>,
    /// Under WSLg the native write lands in the X11 clipboard and succeeds,
    /// but WSLg does not reliably pass it on to Windows, so every copy also
    /// asks the terminal to take it.
    mirror_osc52: bool,
}

impl ClipboardState {
    pub(crate) fn new() -> Self {
        Self {
            native: Clipboard::new().ok(),
            mirror_osc52: detect_wsl(),
        }
    }

    pub(crate) fn copy_text(&mut self, text: &str) -> Result<CopyResult, String> {
        Self::copy_text_impl(
            text,
            self.mirror_osc52,
            |t| self.copy_native(t),
            terminal::copy_to_clipboard,
        )
    }

    fn copy_text_impl<F, G>(
        text: &str,
        mirror_osc52: bool,
        native: F,
        osc52: G,
    ) -> Result<CopyResult, String>
    where
        F: FnOnce(&str) -> Result<(), String>,
        G: FnOnce(&str) -> Result<(), String>,
    {
        if text.is_empty() {
            return Ok(CopyResult::Noop);
        }
        let native_result = native(text);
        if native_result.is_ok() && !mirror_osc52 {
            return Ok(CopyResult::Copied);
        }
        match (native_result, osc52(text)) {
            (Ok(()), _) | (_, Ok(())) => Ok(CopyResult::Copied),
            (Err(native_err), Err(osc52_err)) => {
                Err(format!("native ({native_err}); osc52 ({osc52_err})"))
            }
        }
    }

    fn copy_native(&mut self, text: &str) -> Result<(), String> {
        let Some(clipboard) = &mut self.native else {
            return Err("native clipboard unavailable".into());
        };

        #[cfg(target_os = "linux")]
        {
            let primary = clipboard
                .set()
                .clipboard(LinuxClipboardKind::Primary)
                .text(text);
            let system = clipboard
                .set()
                .clipboard(LinuxClipboardKind::Clipboard)
                .text(text);
            match (primary, system) {
                (_, Ok(())) | (Ok(()), _) => Ok(()),
                (Err(p), Err(s)) => Err(format!("primary: {p}; clipboard: {s}")),
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            clipboard.set_text(text).map_err(|e| e.to_string())
        }
    }
}

fn detect_wsl() -> bool {
    cfg!(target_os = "linux")
        && is_wsl(
            env::var_os(WSL_DISTRO_NAME).is_some(),
            env::var_os(WSL_INTEROP).is_some(),
            fs::read_to_string(OSRELEASE_PATH).ok().as_deref(),
        )
}

fn is_wsl(distro: bool, interop: bool, osrelease: Option<&str>) -> bool {
    distro
        || interop
        || osrelease.is_some_and(|release| release.to_ascii_lowercase().contains(WSL_KERNEL_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use test_case::test_case;

    const TEXT: &str = "hello";
    const NATIVE_ERROR: &str = "no display";
    const OSC52_ERROR: &str = "stdout closed";
    const BOTH_FAILED: &str = "native (no display); osc52 (stdout closed)";
    const OSC52_SKIPPED: &str = "a WSL copy must also reach osc52";
    const WRONG_WSL: &str = "the environment was read as the wrong system";
    const WSL2_RELEASE: &str = "5.15.167.4-microsoft-standard-WSL2";
    const WSL1_RELEASE: &str = "4.4.0-19041-Microsoft";
    const LINUX_RELEASE: &str = "6.8.0-45-generic";

    #[test_case(false ; "outside wsl")]
    #[test_case(true ; "in wsl")]
    fn empty_text_is_noop_and_skips_both_backends(mirror_osc52: bool) {
        let native_called = Cell::new(false);
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "",
            mirror_osc52,
            |_| {
                native_called.set(true);
                Ok(())
            },
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Noop)));
        assert!(!native_called.get(), "empty text must not reach native");
        assert!(!osc52_called.get(), "empty text must not reach osc52");
    }

    #[test]
    fn native_success_skips_osc52() {
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            TEXT,
            false,
            |_| Ok(()),
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(
            !osc52_called.get(),
            "osc52 must not fire after native succeeds"
        );
    }

    #[test]
    fn native_failure_is_recovered_by_osc52() {
        let result =
            ClipboardState::copy_text_impl(TEXT, false, |_| Err(NATIVE_ERROR.into()), |_| Ok(()));
        assert!(matches!(result, Ok(CopyResult::Copied)));
    }

    #[test_case(Ok(()), Ok(()) ; "both backends succeed")]
    #[test_case(Ok(()), Err(OSC52_ERROR) ; "native success covers an osc52 failure")]
    #[test_case(Err(NATIVE_ERROR), Ok(()) ; "osc52 success covers a native failure")]
    fn wsl_copy_reaches_osc52_and_succeeds_when_either_backend_does(
        native: Result<(), &str>,
        osc52: Result<(), &str>,
    ) {
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            TEXT,
            true,
            |_| native.map_err(String::from),
            |_| {
                osc52_called.set(true);
                osc52.map_err(String::from)
            },
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(osc52_called.get(), "{OSC52_SKIPPED}");
    }

    #[test_case(false ; "outside wsl")]
    #[test_case(true ; "in wsl")]
    fn both_failing_composes_errors_with_native_first(mirror_osc52: bool) {
        let result = ClipboardState::copy_text_impl(
            TEXT,
            mirror_osc52,
            |_| Err(NATIVE_ERROR.into()),
            |_| Err(OSC52_ERROR.into()),
        );
        let Err(msg) = result else {
            panic!("expected Err when both backends fail");
        };
        assert_eq!(msg, BOTH_FAILED);
    }

    #[test]
    fn whitespace_only_text_is_not_noop() {
        let native_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "   \n\t",
            false,
            |_| {
                native_called.set(true);
                Ok(())
            },
            |_| Ok(()),
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(
            native_called.get(),
            "whitespace-only text must still be copied"
        );
    }

    #[test_case(true, false, None, true ; "the distro variable alone identifies wsl")]
    #[test_case(false, true, None, true ; "the interop variable alone identifies wsl")]
    #[test_case(false, false, Some(WSL2_RELEASE), true ; "a wsl2 kernel identifies wsl without variables")]
    #[test_case(false, false, Some(WSL1_RELEASE), true ; "a capitalised wsl1 kernel identifies wsl")]
    #[test_case(false, false, Some(LINUX_RELEASE), false ; "a plain linux kernel is not wsl")]
    #[test_case(false, false, None, false ; "an environment with nothing is not wsl")]
    fn wsl_is_recognised_without_its_variables(
        distro: bool,
        interop: bool,
        osrelease: Option<&str>,
        expected: bool,
    ) {
        assert_eq!(is_wsl(distro, interop, osrelease), expected, "{WRONG_WSL}");
    }
}
