use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use reef::TuiApp as App;
use reef::i18n;
use reef::images;
use reef::ui::theme::Theme;
use reef::{editor, input, ui};
use reef_app::{AppEffect, Toast, ViewMode};
use reef_io::agent_deploy::{self, InstallPath, SshSession};
use reef_io::{Backend, LocalBackend, RemoteBackend, ResolvedLocalTarget};
use std::io;
use std::panic;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Minimal argv parsing. Flags recognised:
///   <PATH>                    Positional: open this local directory instead
///                             of the current working directory. Supports
///                             leading `~/` expansion. Mutually exclusive
///                             with `--ssh` / `--agent-exec`.
///   --agent-exec <COMMAND>    Spawn the given shell-split command as a
///                             reef-agent subprocess and drive the UI through
///                             it. Typical value: `ssh host reef-agent --stdio`.
///                             This is the power-user / test hook.
///   --ssh <TARGET>            High-level shortcut. `TARGET` is
///                             `[user@]host[:remote_path]` — reef will:
///                             (a) establish an ssh ControlMaster session,
///                             (b) run the install script to ensure
///                                 `reef-agent` matching this reef's
///                                 version lives on the remote, and
///                             (c) connect to it.
///   --help / -h               Print usage and exit.
fn parse_args() -> Result<AppArgs, String> {
    let mut args = std::env::args().skip(1);
    let mut agent_exec: Option<String> = None;
    let mut ssh_target: Option<String> = None;
    let mut path: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--agent-exec" => {
                agent_exec = Some(
                    args.next()
                        .ok_or_else(|| "--agent-exec requires a value".to_string())?,
                );
            }
            "--ssh" => {
                ssh_target = Some(
                    args.next()
                        .ok_or_else(|| "--ssh requires a [user@]host[:path] value".to_string())?,
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("unexpected extra path argument: {other}"));
                }
                path = Some(other.to_string());
            }
        }
    }
    let modes = [agent_exec.is_some(), ssh_target.is_some(), path.is_some()]
        .into_iter()
        .filter(|b| *b)
        .count();
    if modes > 1 {
        return Err("--agent-exec, --ssh, and <PATH> are mutually exclusive".into());
    }
    Ok(AppArgs {
        agent_exec,
        ssh_target,
        path,
    })
}

fn print_usage() {
    eprintln!("reef — terminal UI for git");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    reef                            # open cwd with local backend");
    eprintln!("    reef <DIR>                      # open DIR (supports ~/)");
    eprintln!(
        "    reef <FILE>                     # open FILE's parent dir, drop straight into 纯预览"
    );
    eprintln!(
        "    reef --ssh user@host            # open remote HOME on host (agent auto-installed)"
    );
    eprintln!("    reef --ssh user@host:/path      # open /path on host");
    eprintln!(
        "    reef --agent-exec \"CMD...\"      # advanced: drive reef through a custom agent pipe"
    );
    eprintln!();
    eprintln!("The --agent-exec value is whitespace-split into argv. Typical remote:");
    eprintln!("    reef --agent-exec \"ssh user@host reef-agent --stdio --workdir /path\"");
}

struct AppArgs {
    agent_exec: Option<String>,
    ssh_target: Option<String>,
    path: Option<String>,
}

/// Split a `[user@]host[:path]` target. Returns `(host_with_user, path)`;
/// `path` defaults to `"."` so the agent opens the remote `$HOME`.
///
/// We keep it simple: split on the *first* `:` because `user@host` itself
/// never contains a colon, and `:` in remote paths is legal but unusual —
/// users with weird paths can always fall through to `--agent-exec`.
fn split_ssh_target(target: &str) -> (String, String) {
    match target.split_once(':') {
        Some((host, "")) => (host.to_string(), ".".to_string()),
        Some((host, path)) => (host.to_string(), path.to_string()),
        None => (target.to_string(), ".".to_string()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    editor::install_editor_resolver();
    let parsed = parse_args()?;

    // Set up panic hook to restore terminal
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        // Best-effort teardown — pop the kbd enhancement flags and disable
        // bracketed paste in case they were enabled. An unmatched pop / a
        // redundant disable is harmless on terminals that never received
        // the matching push (they ignore the CSI sequence).
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        original_hook(panic_info);
    }));

    // Probe terminal background BEFORE raw mode / alt-screen so the OSC 11
    // reply doesn't fragment onto the TUI. `Theme::resolve` also honours the
    // `ui.theme` pref override and a non-TTY fallback.
    let theme = Theme::resolve();

    // Probe image-rendering capabilities using the same pre-raw-mode
    // window: `Picker::from_query_stdio` sends a CSI query and reads the
    // reply on stdin, just like OSC 11 above. `None` means the terminal
    // has no graphics protocol (or the user set REEF_IMAGE_PROTOCOL=off)
    // — image files will render as a friendly metadata card instead.
    let image_picker = images::probe_picker();

    // Build the backend before terminal setup so any spawn failure surfaces
    // as a normal error (stderr) rather than painting half-initialised on
    // the alt-screen.
    //
    // `pending_preview_file` is the relative filename to drop straight into
    // 纯预览 once the first `App` boots — only set on the `reef <file>`
    // quick-look path. Carries across the outer 'session loop only on the
    // very first iteration (subsequent SSH hops shouldn't replay it).
    let mut pending_preview_file: Option<PathBuf> = None;
    let mut backend: Arc<dyn Backend> = if let Some(target) = parsed.ssh_target.as_deref() {
        Arc::new(build_ssh_backend(target)?)
    } else if let Some(spec) = parsed.agent_exec.as_deref() {
        let argv: Vec<String> = spec.split_whitespace().map(|s| s.to_string()).collect();
        if argv.is_empty() {
            return Err("--agent-exec value is empty".into());
        }
        Arc::new(RemoteBackend::spawn(&argv)?)
    } else if let Some(raw) = parsed.path.as_deref() {
        // Switch the process cwd so spawned subprocesses ($EDITOR, git hooks)
        // inherit the same workdir — matches reef-agent's --workdir path.
        match reef_io::resolve_local_target(raw)? {
            ResolvedLocalTarget::Dir(workdir) => {
                std::env::set_current_dir(&workdir)?;
                Arc::new(LocalBackend::open_at(workdir))
            }
            ResolvedLocalTarget::File { workdir, rel } => {
                // Quick-look path: anchor at the enclosing repo root
                // (or the file's parent when no repo is found). `rel` is the
                // path relative to that workdir — Files tab tree + Git
                // tab status both anchor on workdir so the user sees
                // the file in its repo context, not just a subdir.
                std::env::set_current_dir(&workdir)?;
                pending_preview_file = Some(rel);
                Arc::new(LocalBackend::open_at(workdir))
            }
        }
    } else {
        Arc::new(LocalBackend::open_cwd()?)
    };

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // Kitty keyboard protocol: ask the terminal to disambiguate escape
    // sequences so `Alt+Arrow` / `Ctrl+Arrow` / `Shift+Enter` etc. arrive
    // as `KeyEvent { code, modifiers }` instead of being collapsed onto
    // default xterm sequences. Silently ignored by terminals that don't
    // support it (most old ones). Supported today by Ghostty, Kitty,
    // WezTerm, foot, iTerm2 ≥3.5, Alacritty (with --with-flags), etc.
    let kitty_flags_pushed = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        )
    )
    .is_ok();
    // Rename the crossterm backend binding to avoid shadowing the
    // `Arc<dyn Backend>` data-backend constructed above.
    let backend_term = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_term)?;

    // Outer session loop: the Ctrl+O hosts picker emits a switch-session
    // effect to ask for a fresh backend. Each iteration owns one `App` + one backend; the terminal
    // setup/teardown lives outside so the new session inherits the
    // existing alt-screen/mouse-capture state instead of flashing.
    //
    // `pending_connect_error` carries a connect failure from the *previous*
    // iteration into the next App as a toast — `eprintln!` to stderr is
    // invisible while the alt-screen is up, so we route through the toast
    // queue instead.
    let mut pending_connect_error: Option<String> = None;
    'session: loop {
        // App init — clone the picker per session because each App owns
        // it for its preview-protocol lifecycle.
        let mut app = App::new_with_backend(theme, Arc::clone(&backend), image_picker.clone());
        if let Some(msg) = pending_connect_error.take() {
            app.push_toast(Toast::error(msg));
            // Re-open the picker so the user has a visible recovery path
            // — they came here trying to connect to *something*.
            app.open_hosts_picker();
        }
        // `reef <file>` quick-look: reveal the file in the freshly-built
        // tree and enter 纯预览 immediately. Consumed on first iteration
        // so an SSH session swap doesn't try to replay it against a
        // remote tree that doesn't have the path.
        if let Some(file) = pending_preview_file.take() {
            app.enter_focused_preview_with_file(file);
        }
        let mut session_swap_target = None;

        // Main loop
        loop {
            app.tick();
            terminal.draw(|f| ui::render(f, &mut app))?;

            // Block until at least one event arrives (or 16ms timeout for ~60fps)
            if !event::poll(Duration::from_millis(16))? {
                continue;
            }

            // Snapshot selection before processing events
            let sel_before = app.engine.selected_file_identity();

            // Drain ALL pending events so rapid key repeats coalesce — only one
            // render + diff-load runs per frame, regardless of how many events fired.
            loop {
                match event::read()? {
                    Event::Key(key) => {
                        // Crossterm emits Press + Release (and Repeat on hold) for every
                        // physical keystroke on Windows Terminal, in terminals that enable
                        // the kitty keyboard protocol, and on some macOS configurations.
                        // Without this filter each keypress runs handle_key twice, so j/k
                        // navigate 2 rows instead of 1 (and 3+ when held).
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }

                        // Palettes (quick-open / global-search / hosts-picker) and
                        // the full-screen Settings page own every key unconditionally —
                        // 'any key' must not dismiss help while one of them is up — so
                        // route to handle_key first and let it delegate to the page.
                        let snapshot = app.engine.snapshot();
                        if snapshot.overlays.quick_open
                            || snapshot.overlays.global_search
                            || snapshot.overlays.hosts_picker
                            || snapshot.view_mode == ViewMode::Settings
                            || snapshot.view_mode == ViewMode::FocusedPreview
                        {
                            // Full-screen takeovers + overlays own every key,
                            // including the would-be "dismiss help" path:
                            // otherwise a stray j/k inside FocusedPreview
                            // after `h` would close help instead of reaching
                            // handle_key_focused_preview's nav fallthrough.
                            input::handle_key(key, &mut app);
                        } else if snapshot.show_help {
                            app.close_help();
                        } else {
                            input::handle_key(key, &mut app);
                        }
                    }
                    Event::Mouse(mouse) => {
                        input::handle_mouse(mouse, &mut app, &terminal);
                    }
                    Event::Paste(s) => {
                        input::handle_paste(s, &mut app);
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
                // Stop draining if no more events are immediately available
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }

            // After draining, sync selection state
            let sel_after = app.engine.selected_file_identity();
            if sel_after != sel_before {
                app.load_diff();
            }

            let mut should_quit = false;
            for effect in app.engine.drain_effects() {
                match effect {
                    AppEffect::Quit => {
                        should_quit = true;
                    }
                    AppEffect::OpenInEditor(path) => {
                        // Convert an absolute path (input.rs synthesises
                        // `file_tree.root.join(entry.path)`) back to a workdir-
                        // relative form so the backend's remote variant can ship the
                        // path across the ssh boundary verbatim. LocalBackend accepts
                        // either shape.
                        let workdir = app.engine.workdir_path();
                        let rel_for_spec = path
                            .strip_prefix(&workdir)
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|_| path.clone());
                        match app.engine.editor_launch_spec(&rel_for_spec) {
                            Ok(spec) => {
                                if let Err(e) = editor::launch_spec(&mut terminal, &spec) {
                                    app.push_toast(Toast::error(i18n::edit_open_failed(
                                        &e.to_string(),
                                    )));
                                }
                            }
                            Err(e) => {
                                app.push_toast(Toast::error(i18n::edit_open_failed(
                                    &e.to_string(),
                                )));
                            }
                        }
                        // Pick up on-disk changes immediately rather than waiting for
                        // the fs-watcher debounce. `reload_preview_now` bypasses the
                        // scrubbing debounce so the new file contents land on the
                        // first frame after the editor exits, not 80 ms later.
                        app.refresh_file_tree();
                        app.reload_preview_now();
                    }
                    AppEffect::SwitchSession(target) => {
                        session_swap_target = Some(target);
                    }
                    AppEffect::Toast(toast) => app.push_toast(toast),
                    AppEffect::CopyToClipboard {
                        text,
                        success,
                        failure,
                    } => {
                        if let Err(e) = reef::clipboard::copy_to_clipboard(&text) {
                            app.push_toast(Toast::error(format!("{}: {e}", failure.message)));
                        } else if let Some(toast) = success {
                            app.push_toast(toast);
                        }
                    }
                    AppEffect::OpenUrl(url) => {
                        if let Err(e) = App::open_external_url(&url) {
                            app.push_toast(Toast::error(format!("Open link failed: {e}")));
                        } else {
                            app.push_toast(Toast::info(format!("Opened {url}")));
                        }
                    }
                }
            }

            if should_quit {
                break 'session;
            }
            if session_swap_target.is_some() {
                break; // inner loop only — outer 'session handles the swap
            }
        }

        // Session swap: pick up the pending target, build the new backend
        // (still inside raw-mode; the connect failure path below restores
        // it if anything went wrong), then restart the inner loop with a
        // fresh App. Errors here return to the picker state by falling
        // through to a `continue 'session` with the old backend retained.
        if let Some(target) = session_swap_target.take() {
            let target_arg = target.to_arg();
            match build_ssh_backend(&target_arg) {
                Ok(new_backend) => {
                    backend = Arc::new(new_backend);
                    continue 'session;
                }
                Err(e) => {
                    // Surface via the next App's toast queue (stderr
                    // would be eaten by the alt-screen). The previous
                    // backend is retained so the user can retry.
                    pending_connect_error =
                        Some(i18n::ssh_connect_failed(&target_arg, &e.to_string()));
                    continue 'session;
                }
            }
        }
        break 'session;
    } // end 'session

    // Restore terminal. Pop the keyboard enhancement flags only if the
    // push succeeded earlier — popping on a terminal that never received
    // the push is harmless but noisy if we ever change err handling.
    disable_raw_mode()?;
    if kitty_flags_pushed {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// End-to-end `--ssh <target>` flow:
///   1. Resolve host + remote workdir
///   2. Build a ControlMaster-enabled `SshSession`
///   3. Run the install-script probe to ensure the agent is on disk
///   4. Connect to the agent and return the RemoteBackend
fn build_ssh_backend(target: &str) -> Result<RemoteBackend, Box<dyn std::error::Error>> {
    let (host, remote_workdir) = split_ssh_target(target);
    if host.is_empty() {
        return Err("--ssh value is missing a host".into());
    }
    eprintln!(
        "reef: connecting to {host}{}",
        remote_path_suffix(&remote_workdir)
    );

    let session = SshSession::for_host(&host)?;
    let location = agent_deploy::ensure_agent_with_session(
        &session,
        env!("CARGO_PKG_VERSION"),
        agent_deploy::DEFAULT_DOWNLOAD_URL_TEMPLATE,
    )?;

    let via = match location.via {
        InstallPath::AlreadyInstalled => "already installed",
        InstallPath::Downloaded => "downloaded from release",
        InstallPath::Uploaded => "uploaded from local build",
    };
    eprintln!(
        "reef: remote agent ready ({via}) at {}:{}",
        location.host, location.remote_path
    );

    let backend = RemoteBackend::connect_ssh(
        &session,
        &remote_workdir,
        &location.remote_path,
        location.remote_os,
    )?;
    Ok(backend)
}

fn remote_path_suffix(path: &str) -> String {
    if path == "." {
        String::new()
    } else {
        format!(":{path}")
    }
}
