//! Bash install-script generator + output parser.
//!
//! The script is designed to run on a wide range of POSIX hosts — Linux
//! (glibc + musl/alpine), Darwin, FreeBSD — without depending on anything
//! more exotic than `uname`, `mkdir`, `test`, `cat`, `chmod`, one of
//! `wget`/`curl`/`fetch`, and (optionally) `tar`. It's modelled on VSCode
//! Remote SSH's `serverSetup.ts::generateBashInstallScript`, boiled down
//! to the minimum we need for a single-binary agent.
//!
//! # Delimiter protocol
//!
//! The script emits a framed block on stdout so we can ignore motd / PS1
//! noise. The opening and closing markers embed a caller-supplied random
//! id so a clever shell alias or motd can't spoof a fake response:
//!
//! ```text
//! <id>: start
//! exitCode==0==
//! agentPath==/root/.reef/agent/0.6.0/reef-agent==
//! platform==linux==
//! arch==x64==
//! osReleaseId==ubuntu==
//! installState==downloaded==
//! <id>: end
//! ```
//!
//! Values are raw strings between the `==` pair; we never need to embed
//! `==` inside a value, so no escaping is required. Parsers should be
//! tolerant of lines before `<id>: start` and after `<id>: end`.

use std::collections::HashMap;

/// Remote OS family, used to pick between the bash and PowerShell
/// install-script variants. Detected client-side by `mod.rs::probe_remote_os`
/// which runs a tiny `uname -s || ver` probe over ssh before the main
/// install script is dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOs {
    Posix,
    Windows,
}

/// How the remote agent got installed, as reported by the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    /// Binary was already present at the expected path.
    Existed,
    /// Binary was fetched from the download URL (wget/curl/fetch).
    Downloaded,
    /// Download failed — caller should fall through to the upload path.
    DownloadFailed,
    /// Extraction of the downloaded tarball failed.
    ExtractFailed,
    /// Platform/arch detection returned something we don't ship.
    Unsupported,
}

impl InstallState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "existed" => Some(Self::Existed),
            "downloaded" => Some(Self::Downloaded),
            "download_failed" => Some(Self::DownloadFailed),
            "extract_failed" => Some(Self::ExtractFailed),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
        }
    }
}

/// Parsed output of the install script.
#[derive(Debug, Clone)]
pub struct ScriptReport {
    pub exit_code: i32,
    pub agent_path: Option<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub os_release_id: Option<String>,
    pub install_state: Option<InstallState>,
    /// Protocol version reported by the on-disk agent (`reef-agent
    /// --protocol-version`), if we managed to probe it. `None` if the
    /// binary hadn't been installed yet or didn't respond; the install
    /// script emits an empty value for both cases.
    pub protocol_version: Option<String>,
    /// Raw key→value pairs parsed from the delimited block, for debugging
    /// and for forward-compat if the script emits new fields.
    pub raw: HashMap<String, String>,
}

/// Generate a self-contained bash install script.
///
/// Parameters:
/// - `version`: reef/agent semver (`env!("CARGO_PKG_VERSION")` on the caller)
/// - `install_root`: remote shell expression evaluating to a writable
///   directory, e.g. `"$HOME/.reef"`. Must NOT already contain single
///   quotes; we don't escape (the caller controls this value).
/// - `script_id`: unique ASCII id used in the start/end delimiter. 16+
///   random hex chars recommended.
/// - `download_url_template`: URL with `{version}`, `{platform}`, `{arch}`
///   placeholders — e.g.
///   `"https://github.com/reef-tui/reef/releases/download/v{version}/reef-agent-{platform}-{arch}.tar.gz"`
///
/// The returned string is intended to be passed whole as an argument to
/// `bash -c '...'` (via ssh). It does not start with a shebang.
pub fn generate_install_script(
    version: &str,
    install_root: &str,
    script_id: &str,
    download_url_template: &str,
    expected_protocol_version: &str,
) -> String {
    // The script runs through `bash -c` on the remote, so we don't need a
    // shebang. We use `set -u` to catch typos in variable names but
    // deliberately avoid `set -e` — download failures are expected and we
    // want to report them via `installState=download_failed` rather than
    // crashing out silently.
    //
    // `START_MARK`/`END_MARK` are emitted *before and after* anything
    // useful so the client can slice them out of the overall ssh output.
    format!(
        r###"set -u
START_MARK="{script_id}: start"
END_MARK="{script_id}: end"

emit() {{
    printf '%s\n' "$1==$2=="
}}

install_root="{install_root}"
version="{version}"
url_template='{download_url_template}'
expected_proto="{expected_protocol_version}"

# Platform detection — uname -s gives kernel, we fold into the three
# labels reef's release pipeline ships.
uname_s=$(uname -s 2>/dev/null || echo unknown)
case "$uname_s" in
    Linux*) platform=linux ;;
    Darwin*) platform=darwin ;;
    FreeBSD*) platform=freebsd ;;
    *) platform=unknown ;;
esac

uname_m=$(uname -m 2>/dev/null || echo unknown)
case "$uname_m" in
    x86_64|amd64) arch=x64 ;;
    aarch64|arm64) arch=arm64 ;;
    armv7l|armv8l) arch=armhf ;;
    ppc64le) arch=ppc64le ;;
    riscv64) arch=riscv64 ;;
    s390x) arch=s390x ;;
    *) arch=unknown ;;
esac

# musl detection for alpine and friends — libc choice affects which
# static binary the release pipeline publishes.
os_release_id=""
if [ -f /etc/os-release ]; then
    # Safe read: /etc/os-release format is `KEY=value` or `KEY="value"`.
    # We only want the bare ID.
    os_release_id=$(awk -F= '/^ID=/ {{ gsub(/"/, "", $2); print $2; exit }}' /etc/os-release 2>/dev/null)
fi

agent_dir="$install_root/agent/$version"
agent_path="$agent_dir/reef-agent"

# Start of the framed block — everything above this line is diagnostic.
printf '%s\n' "$START_MARK"

emit platform "$platform"
emit arch "$arch"
emit osReleaseId "$os_release_id"
emit agentPath "$agent_path"

if [ "$platform" = "unknown" ] || [ "$arch" = "unknown" ]; then
    emit installState unsupported
    emit exitCode 1
    printf '%s\n' "$END_MARK"
    exit 0
fi

# Protocol-version gate. If a stale agent is sitting at `$agent_path`
# (e.g. left over from a previous reef release that spoke v2 wire
# protocol), rm it so the download/upload path below re-materialises
# the matching binary. We emit `protocolVersion=…` for either case so
# the client can surface what's actually running remotely.
actual_proto=""
if [ -x "$agent_path" ]; then
    actual_proto=$("$agent_path" --protocol-version 2>/dev/null || echo "")
    if [ -n "$expected_proto" ] && [ "$actual_proto" != "$expected_proto" ]; then
        rm -f "$agent_path"
        actual_proto=""
    fi
fi
emit protocolVersion "$actual_proto"

# Idempotency: agent already materialised at the expected version path.
if [ -x "$agent_path" ]; then
    emit installState existed
    emit exitCode 0
    printf '%s\n' "$END_MARK"
    exit 0
fi

# Prepare the target directory *before* probing the network; a
# mkdir failure is fatal for both download and upload paths.
mkdir -p "$agent_dir" 2>/dev/null || {{
    emit installState download_failed
    emit errorMessage "mkdir $agent_dir failed"
    emit exitCode 1
    printf '%s\n' "$END_MARK"
    exit 0
}}

# Resolve the URL by substituting {{version}}/{{platform}}/{{arch}}.
url=$(printf '%s' "$url_template" \
    | sed "s|{{version}}|$version|g" \
    | sed "s|{{platform}}|$platform|g" \
    | sed "s|{{arch}}|$arch|g")

tarball="$agent_dir/reef-agent.tar.gz"

download_ok=0
if command -v wget >/dev/null 2>&1; then
    if wget --tries=3 --timeout=10 -qO "$tarball" "$url"; then
        download_ok=1
    fi
fi
if [ "$download_ok" -ne 1 ] && command -v curl >/dev/null 2>&1; then
    if curl -sSfL --retry 3 --connect-timeout 10 -o "$tarball" "$url"; then
        download_ok=1
    fi
fi
if [ "$download_ok" -ne 1 ] && command -v fetch >/dev/null 2>&1; then
    if fetch -q -o "$tarball" "$url"; then
        download_ok=1
    fi
fi

if [ "$download_ok" -ne 1 ]; then
    # Surface the URL so the client-side log has the exact endpoint that
    # was tried — useful when diagnosing missing releases.
    emit installState download_failed
    emit downloadUrl "$url"
    emit exitCode 0
    printf '%s\n' "$END_MARK"
    exit 0
fi

# Extract. The tarball is expected to contain a single `reef-agent` file
# at the top level; `--strip-components 1` absorbs any prefix directory
# the release pipeline may wrap around it.
if ! tar -xzf "$tarball" -C "$agent_dir" 2>/dev/null; then
    # Try without gzip (some releases ship plain `.tar`).
    if ! tar -xf "$tarball" -C "$agent_dir" 2>/dev/null; then
        emit installState extract_failed
        emit exitCode 0
        printf '%s\n' "$END_MARK"
        exit 0
    fi
fi

# Some release tarballs wrap the binary in an inner directory. Flatten.
if [ ! -x "$agent_path" ]; then
    found=$(find "$agent_dir" -type f -name reef-agent 2>/dev/null | head -1)
    if [ -n "$found" ] && [ "$found" != "$agent_path" ]; then
        mv "$found" "$agent_path"
    fi
fi
chmod +x "$agent_path" 2>/dev/null || true
rm -f "$tarball"

if [ -x "$agent_path" ]; then
    # Re-probe after install — we want the final `protocolVersion==…==`
    # entry to reflect what actually ended up on disk (the earlier emit
    # is empty in the download path).
    actual_proto=$("$agent_path" --protocol-version 2>/dev/null || echo "")
    emit protocolVersion "$actual_proto"
    emit installState downloaded
    emit exitCode 0
else
    emit installState extract_failed
    emit exitCode 1
fi
printf '%s\n' "$END_MARK"
"###,
    )
}

/// Generate a PowerShell install script for Windows remotes. Emits the
/// same `<id>: start/end` delimited block with `key==value==` lines as
/// the bash variant, so `parse_script_output` handles both.
///
/// The script runs via `ssh <host> 'powershell -NoProfile -NonInteractive
/// -Command -'` with the script body passed on stdin (or inlined via
/// `-Command` when the caller escapes correctly). Windows OpenSSH server
/// defaults to cmd.exe; we explicitly spell `powershell` in the caller.
///
/// Parameters match `generate_install_script`. The URL template uses the
/// same `{version}/{platform}/{arch}` placeholders; for Windows
/// `{platform}=windows`, `{arch}=x64|arm64`. The downloaded artifact is
/// expected to be `reef-agent-windows-x64.zip` (matches the release
/// workflow's Windows packaging).
pub fn generate_install_script_powershell(
    version: &str,
    install_root: &str,
    script_id: &str,
    download_url_template: &str,
    expected_protocol_version: &str,
) -> String {
    // `$ErrorActionPreference = "Continue"` so download failures don't
    // throw out of the whole script — we catch them explicitly and emit
    // `installState=download_failed` the same way bash does.
    //
    // Note on quoting: PowerShell interpolates `$(expr)` inside
    // double-quoted strings. We rely on that for the emit helper. All
    // Rust-side values are inlined into the script at generation time
    // and must not contain `"` or `$` characters (callers control these).
    format!(
        r###"$ErrorActionPreference = 'Continue'
$startMark = '{script_id}: start'
$endMark = '{script_id}: end'

function Emit {{
    param([string]$key, [string]$value)
    Write-Host "$key==$value=="
}}

$installRoot = '{install_root}'
# PowerShell doesn't auto-expand `$env:FOO` inside single-quoted strings;
# if the caller passed `$env:USERPROFILE\.reef` we let PS resolve it by
# re-evaluating through the expression parser.
if ($installRoot.Contains('$env:')) {{
    $installRoot = $ExecutionContext.InvokeCommand.ExpandString($installRoot)
}}
$version = '{version}'
$urlTemplate = '{download_url_template}'
$expectedProto = '{expected_protocol_version}'

# Platform is always windows on this code path.
$platform = 'windows'

# Arch mapping: PROCESSOR_ARCHITECTURE is one of AMD64/IA64/ARM64/x86.
switch ($env:PROCESSOR_ARCHITECTURE) {{
    'AMD64' {{ $arch = 'x64' }}
    'IA64'  {{ $arch = 'x64' }}
    'ARM64' {{ $arch = 'arm64' }}
    default {{ $arch = 'unknown' }}
}}

$agentDir = Join-Path $installRoot "agent\$version"
$agentPath = Join-Path $agentDir 'reef-agent.exe'

# Opening delimiter — everything above is diagnostic noise.
Write-Host $startMark

Emit 'platform' $platform
Emit 'arch' $arch
Emit 'osReleaseId' ''
Emit 'agentPath' $agentPath

if ($arch -eq 'unknown') {{
    Emit 'installState' 'unsupported'
    Emit 'exitCode' '1'
    Write-Host $endMark
    exit 0
}}

# Protocol gate: an existing agent whose reported version doesn't match
# `$expectedProto` is evicted so the download/upload path below replaces
# it with a compatible binary.
$actualProto = ''
if (Test-Path $agentPath) {{
    try {{
        $actualProto = (& $agentPath --protocol-version 2>$null | Select-Object -First 1).Trim()
    }} catch {{
        $actualProto = ''
    }}
    if ($expectedProto -ne '' -and $actualProto -ne $expectedProto) {{
        try {{ Remove-Item -Force $agentPath }} catch {{ }}
        $actualProto = ''
    }}
}}
Emit 'protocolVersion' $actualProto

if (Test-Path $agentPath) {{
    Emit 'installState' 'existed'
    Emit 'exitCode' '0'
    Write-Host $endMark
    exit 0
}}

# Prepare target dir.
try {{
    if (-not (Test-Path $agentDir)) {{
        New-Item -ItemType Directory -Path $agentDir -Force | Out-Null
    }}
}} catch {{
    Emit 'installState' 'download_failed'
    Emit 'errorMessage' "mkdir $agentDir failed: $($_.Exception.Message)"
    Emit 'exitCode' '1'
    Write-Host $endMark
    exit 0
}}

# Interpolate URL.
$url = $urlTemplate.Replace('{{version}}', $version).Replace('{{platform}}', $platform).Replace('{{arch}}', $arch)
$zipPath = Join-Path $agentDir 'reef-agent.zip'

# Download. Invoke-WebRequest throws on 404/etc.; treat as download_failed.
$downloadOk = $false
try {{
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zipPath -TimeoutSec 30
    if (Test-Path $zipPath) {{ $downloadOk = $true }}
}} catch {{
    $downloadOk = $false
}}

if (-not $downloadOk) {{
    Emit 'installState' 'download_failed'
    Emit 'downloadUrl' $url
    Emit 'exitCode' '0'
    Write-Host $endMark
    exit 0
}}

# Extract. Windows artifacts are zip; release publishes `.zip`.
try {{
    Expand-Archive -LiteralPath $zipPath -DestinationPath $agentDir -Force
}} catch {{
    Emit 'installState' 'extract_failed'
    Emit 'exitCode' '0'
    Write-Host $endMark
    exit 0
}}

# Some release tarballs wrap the binary in a subdir — flatten.
if (-not (Test-Path $agentPath)) {{
    $found = Get-ChildItem -Path $agentDir -Recurse -Filter 'reef-agent.exe' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($found -and $found.FullName -ne $agentPath) {{
        Move-Item -Force $found.FullName $agentPath
    }}
}}
Remove-Item -Force $zipPath -ErrorAction SilentlyContinue

if (Test-Path $agentPath) {{
    try {{
        $actualProto = (& $agentPath --protocol-version 2>$null | Select-Object -First 1).Trim()
    }} catch {{
        $actualProto = ''
    }}
    Emit 'protocolVersion' $actualProto
    Emit 'installState' 'downloaded'
    Emit 'exitCode' '0'
}} else {{
    Emit 'installState' 'extract_failed'
    Emit 'exitCode' '1'
}}
Write-Host $endMark
"###,
    )
}

/// Parse the stdout of a script invocation into a structured report.
/// Tolerant of whatever noise precedes/follows the delimited block.
pub fn parse_script_output(script_id: &str, stdout: &str) -> Result<ScriptReport, ParseError> {
    let start = format!("{script_id}: start");
    let end = format!("{script_id}: end");

    let start_idx = stdout
        .find(&start)
        .ok_or_else(|| ParseError::Missing(start.clone()))?;
    let rest = &stdout[start_idx + start.len()..];
    let end_idx = rest
        .find(&end)
        .ok_or_else(|| ParseError::Missing(end.clone()))?;
    let inner = &rest[..end_idx];

    let mut raw = HashMap::new();
    for line in inner.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_suffix("==") {
            if let Some((key, value)) = rest.split_once("==") {
                raw.insert(key.to_string(), value.to_string());
            }
        }
    }

    let exit_code = raw
        .get("exitCode")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(1);
    let agent_path = raw.get("agentPath").cloned();
    let platform = raw.get("platform").cloned();
    let arch = raw.get("arch").cloned();
    let os_release_id = raw.get("osReleaseId").cloned().filter(|s| !s.is_empty());
    let install_state = raw
        .get("installState")
        .and_then(|s| InstallState::parse(s.as_str()));
    // Multiple `protocolVersion` emits are possible (once before the
    // idempotency check, once after a fresh download). HashMap keeps the
    // *last* insertion, which is exactly what we want — the post-install
    // value is the authoritative one.
    let protocol_version = raw
        .get("protocolVersion")
        .cloned()
        .filter(|s| !s.is_empty());

    Ok(ScriptReport {
        exit_code,
        agent_path,
        platform,
        arch,
        os_release_id,
        install_state,
        protocol_version,
        raw,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Missing(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Missing(s) => write!(f, "missing delimiter: {s}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Generate a fresh 16-hex-char script id. Uses the OS clock + the
/// process id as entropy sources — good enough for "don't let motd spoof
/// us", not a cryptographic nonce. We deliberately avoid pulling in
/// `rand` for this.
pub fn new_script_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    // Mix and emit 16 hex chars (64 bits of state).
    let mixed = nanos ^ (pid.rotate_left(17));
    let truncated = (mixed as u64) ^ ((mixed >> 64) as u64);
    format!("reef-install-{truncated:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL_TMPL: &str =
        "https://example.com/releases/v{version}/reef-agent-{platform}-{arch}.tar.gz";

    fn mk(version: &str, id: &str) -> String {
        generate_install_script(version, "$HOME/.reef", id, URL_TMPL, "3")
    }

    #[test]
    fn script_contains_script_id_markers() {
        let s = mk("0.6.0", "abc123");
        assert!(s.contains("abc123: start"), "start marker missing");
        assert!(s.contains("abc123: end"), "end marker missing");
    }

    #[test]
    fn script_preserves_version_in_url() {
        let s = mk("0.9.42", "id");
        // The URL is interpolated at *runtime* by the remote shell, not at
        // script-generation time — so the template placeholders survive.
        assert!(
            s.contains("{version}"),
            "template var must be passed through"
        );
        assert!(
            s.contains("0.9.42"),
            "version must be embedded as a bash var"
        );
    }

    #[test]
    fn script_has_idempotent_guard() {
        let s = mk("0.6.0", "id");
        assert!(
            s.contains("if [ -x \"$agent_path\" ]"),
            "script must skip download when binary exists",
        );
    }

    #[test]
    fn script_uses_install_root_without_quoting() {
        // Callers pass the root as a literal bash expression; we embed it
        // verbatim so `$HOME` and `~` expand on the remote.
        let s = generate_install_script("0.6.0", "$HOME/.reef", "id", URL_TMPL, "3");
        assert!(s.contains(r#"install_root="$HOME/.reef""#));
    }

    #[test]
    fn script_declares_each_wget_curl_fetch_downloader() {
        let s = mk("0.6.0", "id");
        assert!(s.contains("command -v wget"));
        assert!(s.contains("command -v curl"));
        assert!(s.contains("command -v fetch"));
    }

    #[test]
    fn script_emits_download_failed_on_no_downloader() {
        let s = mk("0.6.0", "id");
        assert!(s.contains("installState download_failed"));
    }

    #[test]
    fn parse_roundtrip_existed() {
        let id = "id-xyz";
        let stdout = format!(
            "motd blah\n\
             {id}: start\n\
             platform==linux==\n\
             arch==x64==\n\
             osReleaseId==ubuntu==\n\
             agentPath==/home/me/.reef/agent/0.6.0/reef-agent==\n\
             installState==existed==\n\
             exitCode==0==\n\
             {id}: end\n\
             trailing motd\n",
        );
        let report = parse_script_output(id, &stdout).unwrap();
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.platform.as_deref(), Some("linux"));
        assert_eq!(report.arch.as_deref(), Some("x64"));
        assert_eq!(report.os_release_id.as_deref(), Some("ubuntu"));
        assert_eq!(
            report.agent_path.as_deref(),
            Some("/home/me/.reef/agent/0.6.0/reef-agent"),
        );
        assert_eq!(report.install_state, Some(InstallState::Existed));
    }

    #[test]
    fn parse_detects_download_failed_state() {
        let id = "id";
        let stdout = format!(
            "{id}: start\ninstallState==download_failed==\nplatform==linux==\narch==x64==\nexitCode==0==\n{id}: end\n"
        );
        let report = parse_script_output(id, &stdout).unwrap();
        assert_eq!(report.install_state, Some(InstallState::DownloadFailed));
    }

    #[test]
    fn parse_rejects_missing_start_marker() {
        let err = parse_script_output("id", "no delimiters here").unwrap_err();
        assert!(matches!(err, ParseError::Missing(_)));
    }

    #[test]
    fn parse_empty_os_release_id_becomes_none() {
        let id = "id";
        let stdout = format!(
            "{id}: start\nosReleaseId====\nplatform==linux==\narch==x64==\ninstallState==existed==\nexitCode==0==\n{id}: end\n"
        );
        let report = parse_script_output(id, &stdout).unwrap();
        assert_eq!(report.os_release_id, None);
    }

    #[test]
    fn new_script_id_has_stable_prefix() {
        let id = new_script_id();
        assert!(id.starts_with("reef-install-"));
        assert!(id.len() >= 24, "id suspiciously short: {id}");
    }

    /// Syntactic sanity check: feed the generated script to `bash -n`.
    /// Skipped on hosts where `bash` isn't on PATH (extremely rare; bash
    /// is a build-time dep in CI).
    #[test]
    fn script_passes_bash_syntax_check() {
        let bash = matches!(
            std::process::Command::new("bash").arg("--version").output(),
            Ok(o) if o.status.success()
        );
        if !bash {
            eprintln!("skip: bash not available");
            return;
        }

        let script = mk("0.6.0", "syntax-check");
        let mut child = std::process::Command::new("bash")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn bash -n");
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().expect("bash -n wait");
        assert!(
            out.status.success(),
            "bash -n failed:\nscript:\n{script}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // Exploratory property test: varying version / install_root / id must
    // never produce a syntactically broken script.
    #[test]
    fn property_random_inputs_survive_bash_n() {
        let bash_ok = std::process::Command::new("bash")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !bash_ok {
            eprintln!("skip: bash not available");
            return;
        }
        let cases: &[(&str, &str, &str)] = &[
            ("0.0.1", "$HOME/.reef", "abc"),
            ("1.2.3", "$XDG_DATA_HOME/reef", "id-with-dashes"),
            ("99.99.99-rc1", "/tmp/reef-install", "ID_UPPER"),
            ("v-with-dots.9", "$HOME/app", "rand_12abc"),
        ];
        for (version, root, id) in cases {
            let script = generate_install_script(version, root, id, URL_TMPL, "3");
            let mut child = std::process::Command::new("bash")
                .arg("-n")
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "bash -n failed for version={version} root={root} id={id}: {}",
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }

    // ── PowerShell / Windows variant (Track E) ────────────────────────────

    #[test]
    fn powershell_script_contains_script_id_markers() {
        let s = generate_install_script_powershell(
            "0.14.0",
            r"$env:USERPROFILE\.reef",
            "ps-abc",
            URL_TMPL,
            "3",
        );
        assert!(s.contains("ps-abc: start"));
        assert!(s.contains("ps-abc: end"));
    }

    #[test]
    fn powershell_script_has_protocol_version_check() {
        let s = generate_install_script_powershell(
            "0.14.0",
            r"$env:USERPROFILE\.reef",
            "id",
            URL_TMPL,
            "3",
        );
        // `--protocol-version` probe + eviction on mismatch
        assert!(
            s.contains("--protocol-version"),
            "PS script should probe the existing agent's protocol version"
        );
        assert!(
            s.contains("Remove-Item") && s.contains("$actualProto -ne $expectedProto"),
            "PS script should rm the binary when proto mismatches"
        );
    }

    #[test]
    fn powershell_script_uses_expected_emit_format() {
        // `key==value==` delimiter format must match the bash variant so
        // the same `parse_script_output` works on both.
        let s = generate_install_script_powershell(
            "0.14.0",
            r"$env:USERPROFILE\.reef",
            "id",
            URL_TMPL,
            "3",
        );
        assert!(s.contains("Write-Host \"$key==$value==\""));
        assert!(s.contains("Emit 'platform' $platform"));
        assert!(s.contains("Emit 'arch' $arch"));
        assert!(s.contains("Emit 'agentPath' $agentPath"));
        assert!(s.contains("Emit 'installState'"));
    }

    #[test]
    fn powershell_script_maps_known_archs() {
        let s = generate_install_script_powershell(
            "0.14.0",
            r"$env:USERPROFILE\.reef",
            "id",
            URL_TMPL,
            "3",
        );
        assert!(s.contains("'AMD64'"));
        assert!(s.contains("'ARM64'"));
        assert!(s.contains("'IA64'"));
        assert!(s.contains("$arch = 'x64'"));
        assert!(s.contains("$arch = 'arm64'"));
    }

    #[test]
    fn powershell_script_output_parses_like_bash_output() {
        // Simulate a PS script run output (what Windows would emit over
        // stdout) and assert the bash-era parser handles it unchanged.
        let id = "ps-test";
        let stdout = format!(
            "some ps diagnostic\n\
             {id}: start\n\
             platform==windows==\n\
             arch==x64==\n\
             osReleaseId====\n\
             agentPath==C:\\Users\\me\\.reef\\agent\\0.14.0\\reef-agent.exe==\n\
             protocolVersion==3==\n\
             installState==existed==\n\
             exitCode==0==\n\
             {id}: end\n"
        );
        let report = parse_script_output(id, &stdout).unwrap();
        assert_eq!(report.platform.as_deref(), Some("windows"));
        assert_eq!(report.arch.as_deref(), Some("x64"));
        assert_eq!(
            report.agent_path.as_deref(),
            Some(r"C:\Users\me\.reef\agent\0.14.0\reef-agent.exe")
        );
        assert_eq!(report.protocol_version.as_deref(), Some("3"));
        assert_eq!(report.install_state, Some(InstallState::Existed));
        assert_eq!(report.exit_code, 0);
    }
}
