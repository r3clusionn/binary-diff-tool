# Compares bindiff with GNU cmp on large files.
#   ./scripts/bench.ps1 -Dir D:\bindiff-bench -SizeGiB 4 -Runs 5
# Creates a.bin (pseudo-random) and copies of it: same.bin (identical) and late.bin (one byte
# changed 1 MiB before the end, so every tool must read nearly everything). Each command runs once
# to warm the file cache, then -Runs timed times; the median is printed in seconds along with the
# rate over both files. Files are left in place so reruns are fast; delete -Dir when done.
param(
  [string] $Dir = (Join-Path $env:TEMP 'bindiff-bench'),
  [int] $SizeGiB = 4,
  [int] $Runs = 5,
  [string] $Bindiff = (Join-Path $PSScriptRoot '..\target\release\bindiff.exe'),
  [string] $Cmp = 'C:\Program Files\Git\usr\bin\cmp.exe'
)

New-Item -ItemType Directory -Force $Dir | Out-Null
$a = Join-Path $Dir 'a.bin'; $same = Join-Path $Dir 'same.bin'; $late = Join-Path $Dir 'late.bin'
$size = [int64]$SizeGiB * 1GB

if (-not (Test-Path $a) -or (Get-Item $a).Length -ne $size) {
  Write-Host "creating $SizeGiB GiB test files in $Dir ..."
  $rng = [Random]::new(42)
  $buf = New-Object byte[] (64MB)
  $fs = [IO.File]::Create($a)
  for ($i = 0; $i -lt $size / 64MB; $i++) { $rng.NextBytes($buf); $fs.Write($buf, 0, $buf.Length) }
  $fs.Dispose()
  Copy-Item $a $same -Force
  Copy-Item $a $late -Force
  $fs = [IO.File]::OpenWrite($late)
  $fs.Seek($size - 1MB, 'Begin') | Out-Null
  $fs.WriteByte(0x5a) | Out-Null
  $fs.Dispose()
}

function Measure-Cmd([string] $cmdline) {
  cmd /c "$cmdline > NUL 2>&1" | Out-Null
  $t = 1..$Runs | ForEach-Object {
    $sw = [Diagnostics.Stopwatch]::StartNew(); cmd /c "$cmdline > NUL 2>&1" | Out-Null; $sw.Stop(); $sw.Elapsed.TotalSeconds
  } | Sort-Object
  $t[[int][math]::Floor($Runs / 2)]
}

$bd = '"' + $Bindiff + '"'; $cm = '"' + $Cmp + '"'
$cases = @(
  @{ Name = 'bindiff read (default)';  Cmd = { param($b) "$bd $a $b" } },
  @{ Name = 'bindiff mmap';          Cmd = { param($b) "$bd --io mmap $a $b" } },
  @{ Name = 'bindiff read, 1 thread'; Cmd = { param($b) "$bd -j 1 $a $b" } },
  @{ Name = 'bindiff --hash (BLAKE3)'; Cmd = { param($b) "$bd --hash $a $b" } },
  @{ Name = 'GNU cmp';               Cmd = { param($b) "$cm $a $b" } }
)
Write-Host ("`n{0} GiB per file, median of {1} runs, warm cache" -f $SizeGiB, $Runs)
Write-Host ('{0,-26} {1,22} {2,22}' -f 'tool', 'identical files', 'one byte differs near end')
foreach ($c in $cases) {
  $row = foreach ($other in $same, $late) {
    $s = Measure-Cmd (& $c.Cmd $other)
    '{0,6:N2} s  {1,6:N2} GB/s' -f $s, (2 * $size / 1e9 / $s)
  }
  Write-Host ('{0,-26} {1,22} {2,22}' -f $c.Name, $row[0], $row[1])
}
