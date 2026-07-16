#!/usr/bin/env node
// Runs once after npm install/update. Reads the binary from the matching
// per-platform package and installs it under the Open Grok namespace:
//
//   Unix:    open-grok-<version>  +  open-grok  (symlink)
//   Windows: open-grok-<version>.exe  +  open-grok.exe  (copy)
//
// Versioned files ensure running processes are never disrupted on macOS
// (replacing a binary that a running process has mmap'd causes SIGKILL
// because the kernel can no longer verify the code signature).
const path = require('path');
const fs = require('fs');
const os = require('os');
const zlib = require('zlib');

const OPENGROK_HOME = process.env.OPENGROK_HOME || path.join(os.homedir(), '.opengrok');
const CANONICAL_DIR = path.join(OPENGROK_HOME, 'bin');

const key = `${process.platform}-${process.arch}`;
const SUPPORTED = new Set([
    'darwin-arm64',
    'darwin-x64',
    'linux-x64',
    'linux-arm64',
    'win32-x64',
    'win32-arm64',
]);
if (!SUPPORTED.has(key)) {
    console.error(`open-grok: unsupported platform ${key}`);
    process.exit(0);
}

// Resolve the per-platform sibling package's directory. The matching
// optionalDependency is installed by npm based on `os`/`cpu` filters; the
// other five are silently skipped. If the matching one is missing, npm was
// likely invoked with --no-optional or the platform is unsupported.
function resolvePlatformPackageDir() {
    const platformPkg = `@mweinbach/open-grok-${key}`;
    try {
        return path.dirname(require.resolve(`${platformPkg}/package.json`));
    } catch {
        return null;
    }
}

let version;
try { version = require('../package.json').version; } catch {}
if (!version) {
    console.error('open-grok: unable to determine version');
    process.exit(0);
}

const IS_WINDOWS = process.platform === 'win32';
const EXE = IS_WINDOWS ? '.exe' : '';

fs.mkdirSync(CANONICAL_DIR, { recursive: true });

// Install a vendored binary: versioned filename + symlink (Unix) or copy (Windows).
// Binaries are shipped brotli-compressed in the per-platform npm tarball to keep
// each sub-package well under npm's ~200 MB tarball limit. This function
// decompresses them before installing into the canonical layout.
function installBinary(binName, sourceDir, vendorSubpath) {
    const brPath = path.join(sourceDir, 'bin', vendorSubpath + '.br');
    const rawPath = path.join(sourceDir, 'bin', vendorSubpath);
    let vendoredBinPath;
    if (fs.existsSync(brPath)) {
        const compressed = fs.readFileSync(brPath);
        const decompressed = zlib.brotliDecompressSync(compressed);
        vendoredBinPath = rawPath;
        fs.writeFileSync(vendoredBinPath, decompressed);
        if (!IS_WINDOWS) fs.chmodSync(vendoredBinPath, 0o755);
        try { fs.unlinkSync(brPath); } catch {}
    } else if (fs.existsSync(rawPath)) {
        vendoredBinPath = rawPath;
    } else {
        console.error(`open-grok: missing binary at ${brPath}`);
        return false;
    }

    const versionedName = `${binName}-${version}${EXE}`;
    const versionedPath = path.join(CANONICAL_DIR, versionedName);
    const canonicalName = `${binName}${EXE}`;
    const canonicalPath = path.join(CANONICAL_DIR, canonicalName);

    // Only copy if this exact version isn't already installed.
    if (!fs.existsSync(versionedPath)) {
        const tmpPath = versionedPath + `.tmp.${process.pid}`;
        try {
            fs.copyFileSync(vendoredBinPath, tmpPath);
            if (!IS_WINDOWS) fs.chmodSync(tmpPath, 0o755);
            fs.renameSync(tmpPath, versionedPath);
        } finally {
            try { fs.unlinkSync(tmpPath); } catch {}
        }
    }

    if (IS_WINDOWS) {
        // Symlinks need elevation on Windows; copy instead. If the exe is
        // locked by a running process, rename it aside then retry.
        const oldPath = canonicalPath + '.old';
        try { fs.unlinkSync(oldPath); } catch {} // stale backup from prior update
        try {
            try { fs.unlinkSync(canonicalPath); } catch {}
            fs.copyFileSync(versionedPath, canonicalPath);
        } catch (e) {
            try {
                fs.renameSync(canonicalPath, oldPath);
                try {
                    fs.copyFileSync(versionedPath, canonicalPath);
                } catch (copyErr) {
                    // Rollback: restore the old binary so the install isn't broken.
                    try { fs.renameSync(oldPath, canonicalPath); } catch {}
                    throw copyErr;
                }
            } catch (e2) {
                console.error(`open-grok: failed to update ${canonicalPath}: ${e2.message}`);
                console.error('Close all running Open Grok processes and try again.');
                return false;
            }
        }
    } else {
        // Atomic symlink swap.
        const tmpLink = canonicalPath + `.link.${process.pid}`;
        try { fs.unlinkSync(tmpLink); } catch {}
        fs.symlinkSync(versionedName, tmpLink);
        fs.renameSync(tmpLink, canonicalPath);
    }

    console.log(`${binName} ${version} installed to ${canonicalPath} -> ${versionedName}`);
    return true;
}

// Best-effort cleanup of old versioned binaries for a given binary name.
// Keeps the current version and the previous one (in case a process is still
// running the old binary and hasn't fully loaded all pages yet).
// Uses an exact prefix match + hyphen + digit.
function cleanupOldVersions(binName) {
    try {
        const prefix = `${binName}-`;
        const currentVersioned = `${binName}-${version}${EXE}`;
        const entries = fs.readdirSync(CANONICAL_DIR);
        const versionedBinaries = entries
            .filter(e => {
                if (!e.startsWith(prefix)) return false;
                if (e.includes('.tmp.') || e.includes('.link.')) return false;
                if (e === currentVersioned) return false;
                const suffix = e.slice(prefix.length);
                return /^\d/.test(suffix);
            })
            .sort((a, b) => {
                const parseParts = (entry) => {
                    const match = entry.slice(prefix.length).match(/^(\d+)\.(\d+)\.(\d+)/);
                    return match ? match.slice(1).map(Number) : [0, 0, 0];
                };
                const pa = parseParts(a);
                const pb = parseParts(b);
                for (let i = 0; i < 3; i++) {
                    if ((pa[i] || 0) !== (pb[i] || 0)) return (pb[i] || 0) - (pa[i] || 0);
                }
                return 0;
            });
        for (const old of versionedBinaries.slice(1)) {
            try { fs.unlinkSync(path.join(CANONICAL_DIR, old)); } catch {}
        }
    } catch {}
}

const platformDir = resolvePlatformPackageDir();
if (!platformDir) {
    console.error(`open-grok: platform package @mweinbach/open-grok-${key} not installed.`);
    console.error('  This usually means npm was invoked with --no-optional, or the install failed.');
    console.error('  Try reinstalling the open-grok npm package with optional dependencies enabled.');
    process.exit(0);
}

installBinary('open-grok', platformDir, `open-grok${EXE}`);
cleanupOldVersions('open-grok');

// Shell completions: print setup hints (no silent shell config mutation).
// Set OPENGROK_INSTALL_COMPLETIONS=1 to auto-generate under OPENGROK_HOME.
const OPEN_GROK_PATH = path.join(CANONICAL_DIR, `open-grok${EXE}`);
if (process.env.OPENGROK_INSTALL_COMPLETIONS === '1' && !IS_WINDOWS) {
    try {
        const { spawnSync } = require('child_process');
        const completionsDir = path.join(OPENGROK_HOME, 'completions');
        const bashPath = path.join(completionsDir, 'bash', 'open-grok.bash');
        const zshPath = path.join(completionsDir, 'zsh', '_open-grok');
        fs.mkdirSync(path.dirname(bashPath), { recursive: true });
        fs.mkdirSync(path.dirname(zshPath), { recursive: true });
        const bashRes = spawnSync(OPEN_GROK_PATH, ['completions', 'bash'], { encoding: 'utf8' });
        if (bashRes.status === 0) fs.writeFileSync(bashPath, bashRes.stdout);
        const zshRes = spawnSync(OPEN_GROK_PATH, ['completions', 'zsh'], { encoding: 'utf8' });
        if (zshRes.status === 0) fs.writeFileSync(zshPath, zshRes.stdout);
        console.log(`Completions generated to ${completionsDir} (bash/zsh)`);
    } catch {}
} else if (!IS_WINDOWS) {
    console.log('Tip: open-grok completions bash > ~/.local/share/bash-completion/completions/open-grok');
    console.log('     open-grok completions zsh  > ~/.zsh/completions/_open-grok');
}
