'use strict';
// This helper lives in userData, outside the directory it replaces. It starts
// before the old Electron exits. Linux permits renaming its mapped executable.
const fs = require('node:fs');
const fsp = fs.promises;
const path = require('node:path');
const { spawn } = require('node:child_process');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
function alive(pid) { try { process.kill(pid, 0); return true; } catch (e) { return e.code === 'EPERM'; } }
async function validateJob(job, jobFile) {
  const parent = path.dirname(job.installDir || '');
  const cache = path.dirname(jobFile);
  if (!/^[a-f0-9]{48}$/.test(job.token) || !Number.isSafeInteger(job.parentPid) || job.parentPid < 1
    || !Number.isInteger(job.timeout) || job.timeout < 100 || job.timeout > 120_000
    || !path.isAbsolute(job.installDir) || path.parse(job.installDir).root === job.installDir
    || job.stage !== path.join(parent, `.spark-update-${job.token}`)
    || job.backup !== path.join(parent, `.spark-backup-${job.token}`)
    || !job.source.startsWith(job.stage + path.sep) || path.basename(job.executable) !== job.executable
    || ['ack', 'started', 'cancel'].some(key => job[key] !== path.join(cache, `${job.token}.${key}`))) throw new Error('Некорректное задание обновления');
  for (const directory of [job.installDir, job.stage, job.source]) {
    const stat = await fsp.lstat(directory);
    if (!stat.isDirectory() || stat.isSymbolicLink()) throw new Error('Небезопасный каталог обновления');
  }
}
function launch(job) {
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  const child = spawn(path.join(job.installDir, job.executable), job.launchArgs || [], { detached: true, stdio: 'ignore', cwd: job.installDir, env });
  child.on('error', () => {}); child.unref(); return child;
}
async function runJob(jobFile) {
  // This independent Node helper only operates on physical installation files.
  // Setting this here keeps importing powershellScript in the main app safe.
  process.noAsar = true;
  const job = JSON.parse(await fsp.readFile(jobFile, 'utf8'));
  await validateJob(job, jobFile);
  let moved = false, replacement = false, child;
  await fsp.writeFile(job.started, 'started', { mode: 0o600 });
  try {
    const waitUntil = Date.now() + 120_000;
    while (alive(job.parentPid)) {
      if (fs.existsSync(job.cancel)) return;
      if (Date.now() > waitUntil) throw new Error('Старый Spark не завершился');
      await sleep(100);
    }
    if (fs.existsSync(job.cancel)) return;
    await fsp.rename(job.installDir, job.backup); moved = true;
    await fsp.rename(job.source, job.installDir); replacement = true;
    child = launch(job);
    const deadline = Date.now() + job.timeout;
    while (Date.now() < deadline) {
      if (fs.existsSync(job.ack) && await fsp.readFile(job.ack, 'utf8') === job.token) {
        await fsp.rm(job.backup, { recursive: true, force: true }).catch(() => {});
        moved = false;
        return;
      }
      await sleep(100);
    }
    throw new Error('Новая версия не подтвердила запуск; восстановлена предыдущая версия');
  } catch (error) {
    await fsp.writeFile(jobFile + '.error', error.message, { mode: 0o600 });
    if (child?.pid && alive(child.pid)) {
      try { process.kill(-child.pid, 'SIGTERM'); } catch {}
      for (let i = 0; i < 20 && alive(child.pid); i++) await sleep(50);
      if (alive(child.pid)) { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }
    }
    if (replacement) await fsp.rename(job.installDir, job.source);
    if (moved) { await fsp.rename(job.backup, job.installDir); launch(job); }
    else if (fs.existsSync(path.join(job.installDir, job.executable))) launch(job);
  } finally {
    await fsp.rm(job.stage, { recursive: true, force: true });
    for (const file of [job.started, job.ack, job.cancel, jobFile]) await fsp.rm(file, { force: true });
  }
}

// PowerShell is a Windows system component, so replacing a portable directory
// does not keep Spark.exe locked. Installed copies use NSIS to preserve registry
// entries, shortcuts, and the uninstaller rather than swapping program files.
const powershellScript = String.raw`param([Parameter(Mandatory=$true)][string]$JobFile)
$ErrorActionPreference = 'Stop'
$job = Get-Content -LiteralPath $JobFile -Raw | ConvertFrom-Json
$parent = [IO.Path]::GetDirectoryName($job.installDir)
$cache = [IO.Path]::GetDirectoryName($JobFile)
if ($job.token -notmatch '^[a-f0-9]{48}$' -or $job.parentPid -lt 1 -or $job.timeout -lt 100 -or $job.timeout -gt 120000 -or
    -not [IO.Path]::IsPathRooted($job.installDir) -or $job.installDir -eq [IO.Path]::GetPathRoot($job.installDir) -or
    $job.stage -ne (Join-Path $parent ('.spark-update-' + $job.token)) -or
    $job.backup -ne (Join-Path $parent ('.spark-backup-' + $job.token)) -or
    -not $job.source.StartsWith(($job.stage + [IO.Path]::DirectorySeparatorChar), [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetFileName($job.executable) -ne $job.executable) { throw 'Invalid update job' }
foreach ($kind in @('ack', 'started', 'cancel')) { if ($job.$kind -ne (Join-Path $cache ($job.token + '.' + $kind))) { throw 'Invalid update job path' } }
foreach ($directory in @($job.installDir, $job.stage, $job.source)) {
  $item = Get-Item -LiteralPath $directory -Force
  if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Unsafe update directory' }
}
$moved = $false; $replacement = $false; $child = $null
function Move-WithRetry([string]$from, [string]$to) {
  $until = [DateTime]::UtcNow.AddSeconds(10)
  while ($true) {
    try { Move-Item -LiteralPath $from -Destination $to; return } catch { if ([DateTime]::UtcNow -ge $until) { throw }; Start-Sleep -Milliseconds 200 }
  }
}
function Launch-Spark {
  if ($job.launchArgs) { return Start-Process -FilePath (Join-Path $job.installDir $job.executable) -WorkingDirectory $job.installDir -ArgumentList $job.launchArgs -PassThru }
  return Start-Process -FilePath (Join-Path $job.installDir $job.executable) -WorkingDirectory $job.installDir -PassThru
}
[IO.File]::WriteAllText($job.started, 'started')
try {
  $deadline = [DateTime]::UtcNow.AddSeconds(120)
  while (Get-Process -Id $job.parentPid -ErrorAction SilentlyContinue) {
    if (Test-Path -LiteralPath $job.cancel) { return }
    if ([DateTime]::UtcNow -gt $deadline) { throw 'Old Spark did not exit' }
    Start-Sleep -Milliseconds 100
  }
  if (Test-Path -LiteralPath $job.cancel) { return }
  if ($job.installer) {
    # NSIS /D must be the final argument, without surrounding quotes, including
    # for paths containing spaces. Start-Process supplies a raw command line.
    $setup = Start-Process -FilePath $job.installer -ArgumentList ('/S --updated /D=' + $job.installDir) -Wait -PassThru
    if ($setup.ExitCode -ne 0) { throw ('Installer exit code: ' + $setup.ExitCode) }
    $child = Launch-Spark
  } else {
    Move-WithRetry $job.installDir $job.backup
    $moved = $true
    Move-WithRetry $job.source $job.installDir
    $replacement = $true
    $child = Launch-Spark
  }
  $deadline = [DateTime]::UtcNow.AddMilliseconds($job.timeout)
  while ([DateTime]::UtcNow -lt $deadline) {
    if ((Test-Path -LiteralPath $job.ack) -and ([IO.File]::ReadAllText($job.ack) -eq $job.token)) {
      if ($moved) { Remove-Item -LiteralPath $job.backup -Recurse -Force -ErrorAction SilentlyContinue; $moved = $false }
      return
    }
    Start-Sleep -Milliseconds 100
  }
  throw 'New Spark did not acknowledge startup'
} catch {
  $failure = $_.Exception.Message
  [IO.File]::WriteAllText(($JobFile + '.error'), $failure)
  if ($child -and -not $child.HasExited) { & taskkill.exe /PID $child.Id /T /F 2>$null | Out-Null; $child.WaitForExit() }
  if ($replacement) { Move-WithRetry $job.installDir $job.source }
  if ($moved) { Move-WithRetry $job.backup $job.installDir; $null = Launch-Spark }
  elseif (Test-Path -LiteralPath (Join-Path $job.installDir $job.executable)) { $null = Launch-Spark }
} finally {
  if (Test-Path -LiteralPath $job.stage) { Remove-Item -LiteralPath $job.stage -Recurse -Force -ErrorAction SilentlyContinue }
  foreach ($file in @($job.started, $job.ack, $job.cancel, $JobFile)) { if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -Force } }
}
`;
module.exports = { runJob, powershellScript };
if (require.main === module) runJob(process.argv[2]).catch(error => { console.error(error.message); process.exitCode = 1; });
