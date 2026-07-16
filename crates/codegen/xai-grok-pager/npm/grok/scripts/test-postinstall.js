#!/usr/bin/env node

// Contract tests for the Open Grok npm wrapper. These tests intentionally
// focus on the public executable, state namespace, and fork-owned donor
// package contract.

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');

const packageRoot = path.resolve(__dirname, '..');
const binSourceDir = path.join(packageRoot, 'bin');
const packageJson = JSON.parse(fs.readFileSync(path.join(packageRoot, 'package.json'), 'utf8'));
const postinstallSource = fs.readFileSync(path.join(binSourceDir, 'postinstall.js'), 'utf8');
const trampolineSource = fs.readFileSync(path.join(binSourceDir, 'open-grok'), 'utf8');
const assemblerSource = fs.readFileSync(
    path.join(packageRoot, 'scripts', 'assemble-platform-packages.js'),
    'utf8',
);

let passed = 0;
let failed = 0;

function test(name, fn) {
    try {
        fn();
        console.log(`  ✓ ${name}`);
        passed += 1;
    } catch (error) {
        console.error(`  ✗ ${name}`);
        console.error(`    ${error.stack || error.message}`);
        failed += 1;
    }
}

function makeTmpDir() {
    return fs.mkdtempSync(path.join(os.tmpdir(), 'open-grok-npm-test-'));
}

function cleanup(dir) {
    fs.rmSync(dir, { recursive: true, force: true });
}

function installVersionedBinary(source, version, binDir) {
    fs.mkdirSync(binDir, { recursive: true });
    const versionedName = `open-grok-${version}`;
    const versionedPath = path.join(binDir, versionedName);
    const canonicalPath = path.join(binDir, 'open-grok');

    if (!fs.existsSync(versionedPath)) {
        const temporaryPath = `${versionedPath}.tmp.${process.pid}`;
        try {
            fs.copyFileSync(source, temporaryPath);
            fs.chmodSync(temporaryPath, 0o755);
            fs.renameSync(temporaryPath, versionedPath);
        } finally {
            try { fs.unlinkSync(temporaryPath); } catch {}
        }
    }

    const temporaryLink = `${canonicalPath}.link.${process.pid}`;
    try { fs.unlinkSync(temporaryLink); } catch {}
    fs.symlinkSync(versionedName, temporaryLink);
    fs.renameSync(temporaryLink, canonicalPath);

    return { canonicalPath, versionedName, versionedPath };
}

function versionParts(entry) {
    const match = entry.match(/^open-grok-(\d+)\.(\d+)\.(\d+)/);
    return match ? match.slice(1).map(Number) : [0, 0, 0];
}

function cleanupOldVersions(binDir, currentVersion) {
    const current = `open-grok-${currentVersion}`;
    const candidates = fs.readdirSync(binDir)
        .filter((entry) => (
            /^open-grok-\d/.test(entry)
            && !entry.includes('.tmp.')
            && !entry.includes('.link.')
            && entry !== current
        ))
        .sort((a, b) => {
            const left = versionParts(a);
            const right = versionParts(b);
            for (let index = 0; index < 3; index += 1) {
                if (left[index] !== right[index]) return right[index] - left[index];
            }
            return 0;
        });

    for (const old of candidates.slice(1)) {
        fs.unlinkSync(path.join(binDir, old));
    }
}

console.log('Open Grok npm namespace tests\n');

test('package exposes only the open-grok command', () => {
    assert.strictEqual(packageJson.name, '@mweinbach/open-grok');
    assert.deepStrictEqual(packageJson.bin, { 'open-grok': 'bin/open-grok' });
    assert.ok(!Object.hasOwn(packageJson.bin, 'grok'));
    assert.ok(!Object.hasOwn(packageJson.bin, 'agent'));
});

test('all platform packages use the fork scope and matching version', () => {
    const dependencies = Object.entries(packageJson.optionalDependencies || {});
    assert.strictEqual(dependencies.length, 6);
    for (const [name, version] of dependencies) {
        assert.match(name, /^@mweinbach\/open-grok-(?:darwin|linux|win32)-(?:arm64|x64)$/);
        assert.strictEqual(version, packageJson.version);
    }
    assert.doesNotMatch(trampolineSource, /@xai-official\/grok/);
    assert.doesNotMatch(postinstallSource, /@xai-official\/grok/);
});

test('platform assembler resolves only the canonical Open Grok build artifact', () => {
    assert.doesNotMatch(assemblerSource, /['"]xai-grok-pager(?:\.exe)?['"]/);
    assert.strictEqual(
        (assemblerSource.match(/['"]open-grok['"]/g) || []).length,
        8,
    );
    assert.strictEqual(
        (assemblerSource.match(/['"]open-grok\.exe['"]/g) || []).length,
        4,
    );
});

test('package bin directory contains no grok or agent trampoline', () => {
    assert.deepStrictEqual(fs.readdirSync(binSourceDir).sort(), ['open-grok', 'postinstall.js']);
});

test('trampoline and postinstall honor OPENGROK_HOME', () => {
    assert.match(trampolineSource, /process\.env\.OPENGROK_HOME/);
    assert.match(postinstallSource, /process\.env\.OPENGROK_HOME/);
    assert.doesNotMatch(trampolineSource, /process\.env\.(?:GROK_HOME|XAI_GROK_HOME)/);
    assert.doesNotMatch(postinstallSource, /process\.env\.(?:GROK_HOME|XAI_GROK_HOME)/);
});

test('postinstall installs and cleans only open-grok', () => {
    assert.match(postinstallSource, /installBinary\('open-grok'/);
    assert.match(postinstallSource, /cleanupOldVersions\('open-grok'\)/);
    assert.doesNotMatch(postinstallSource, /installBinary\('(?:grok|agent)'/);
    assert.doesNotMatch(postinstallSource, /cleanupOldVersions\('(?:grok|agent|grok-pager)'\)/);
});

test('npm marker uses the fork-specific environment namespace', () => {
    assert.match(trampolineSource, /OPENGROK_MANAGED_BY_NPM/);
    assert.doesNotMatch(trampolineSource, /[^A-Z]GROK_MANAGED_BY_NPM/);
});

test('fresh install creates open-grok and a versioned target', () => {
    const dir = makeTmpDir();
    try {
        const source = path.join(dir, 'source');
        const binDir = path.join(dir, '.opengrok', 'bin');
        fs.writeFileSync(source, 'open-grok-binary');
        const result = installVersionedBinary(source, '0.1.220-open-grok.3', binDir);

        assert.strictEqual(fs.readlinkSync(result.canonicalPath), result.versionedName);
        assert.strictEqual(fs.readFileSync(result.canonicalPath, 'utf8'), 'open-grok-binary');
        assert.strictEqual(fs.statSync(result.versionedPath).mode & 0o777, 0o755);
        assert.ok(!fs.existsSync(path.join(binDir, 'grok')));
        assert.ok(!fs.existsSync(path.join(binDir, 'agent')));
    } finally {
        cleanup(dir);
    }
});

test('upgrade is atomic and keeps the previous version', () => {
    const dir = makeTmpDir();
    try {
        const source = path.join(dir, 'source');
        const binDir = path.join(dir, 'bin');
        fs.writeFileSync(source, 'first');
        installVersionedBinary(source, '0.1.219-open-grok.1', binDir);
        fs.writeFileSync(source, 'second');
        const latest = installVersionedBinary(source, '0.1.220-open-grok.3', binDir);

        assert.strictEqual(fs.readlinkSync(latest.canonicalPath), latest.versionedName);
        assert.strictEqual(fs.readFileSync(latest.canonicalPath, 'utf8'), 'second');
        assert.ok(fs.existsSync(path.join(binDir, 'open-grok-0.1.219-open-grok.1')));
        assert.deepStrictEqual(
            fs.readdirSync(binDir).filter((entry) => entry.includes('.tmp.') || entry.includes('.link.')),
            [],
        );
    } finally {
        cleanup(dir);
    }
});

test('cleanup handles fork prerelease versions and leaves upstream commands untouched', () => {
    const dir = makeTmpDir();
    try {
        const binDir = path.join(dir, 'bin');
        fs.mkdirSync(binDir, { recursive: true });
        for (const version of [
            '0.1.218-open-grok.1',
            '0.1.219-open-grok.1',
            '0.1.220-open-grok.3',
        ]) {
            fs.writeFileSync(path.join(binDir, `open-grok-${version}`), version);
        }
        fs.writeFileSync(path.join(binDir, 'grok'), 'upstream-grok');
        fs.writeFileSync(path.join(binDir, 'agent'), 'upstream-agent');

        cleanupOldVersions(binDir, '0.1.220-open-grok.3');

        assert.ok(fs.existsSync(path.join(binDir, 'open-grok-0.1.220-open-grok.3')));
        assert.ok(fs.existsSync(path.join(binDir, 'open-grok-0.1.219-open-grok.1')));
        assert.ok(!fs.existsSync(path.join(binDir, 'open-grok-0.1.218-open-grok.1')));
        assert.strictEqual(fs.readFileSync(path.join(binDir, 'grok'), 'utf8'), 'upstream-grok');
        assert.strictEqual(fs.readFileSync(path.join(binDir, 'agent'), 'utf8'), 'upstream-agent');
    } finally {
        cleanup(dir);
    }
});

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
