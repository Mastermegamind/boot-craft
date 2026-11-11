# End-to-end test on Windows with a VHD attached as a disk, which Windows
# reports as "File Backed Virtual": no real drive is touched. Needs an
# elevated (administrator) PowerShell; build first:
#
#   cargo build; .\scripts\windows-test.ps1
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$bin = if ($env:BIN) { $env:BIN } else { (Resolve-Path 'target\debug\boot-craft.exe').Path }

$work = Join-Path ([IO.Path]::GetTempPath()) ("boot-craft-" + [guid]::NewGuid())
New-Item -ItemType Directory $work | Out-Null
$vhd = Join-Path $work 'drive.vhd'

function Pass($message) { Write-Host "PASS: $message" }
function Fail($message) { Write-Host "FAIL: $message"; exit 1 }

function DiskPart([string[]] $lines) {
    $script = Join-Path $work 'diskpart.txt'
    Set-Content $script $lines -Encoding ascii
    $out = diskpart /s $script | Out-String
    if ($LASTEXITCODE -ne 0) { Write-Host $out; throw "diskpart failed" }
    $out
}

function Attach {
    DiskPart @("select vdisk file=`"$vhd`"", "attach vdisk") | Out-Null
    Start-Sleep -Seconds 2
    $disk = Get-Disk | Where-Object { $_.Location -eq $vhd -or $_.BusType -eq 'File Backed Virtual' } | Select-Object -Last 1
    if (-not $disk) { throw "the VHD didn't show up as a disk" }
    "PhysicalDrive$($disk.Number)"
}

# A fixed VHD is the disk's bytes followed by a footer: detach it and compare
# its start with the image, without trusting boot-craft's own read-back.
function Compare-WithImage([string] $what) {
    DiskPart @("select vdisk file=`"$vhd`"", "detach vdisk") | Out-Null
    $stream = [IO.File]::OpenRead($vhd)
    try {
        $written = New-Object byte[] $image.Length
        $read = 0
        while ($read -lt $written.Length) {
            $n = $stream.Read($written, $read, $written.Length - $read)
            if ($n -eq 0) { break }
            $read += $n
        }
    } finally { $stream.Dispose() }
    if (-not [Linq.Enumerable]::SequenceEqual([byte[]]$written, [byte[]]$image)) { Fail "${what}: the disk doesn't match the image" }
    Attach
}

function Write-Image([string] $file, [string] $drive, [string[]] $extra) {
    $out = & $bin write $file "\\.\$drive" --allow-loop --yes @extra 2>&1 | Out-String
    @{ ok = ($LASTEXITCODE -eq 0); out = $out }
}

try {
    # A 40 MiB "disk image": random data with a boot-sector signature and an
    # empty partition table (so Windows mounts nothing from it), four bytes
    # longer than whole sectors so the last write is a partial sector.
    $image = New-Object byte[] (40MB + 4)
    (New-Object Random 7).NextBytes($image)
    for ($i = 446; $i -lt 510; $i++) { $image[$i] = 0 }
    $image[510] = 0x55; $image[511] = 0xAA
    $imageFile = Join-Path $work 'image.img'
    [IO.File]::WriteAllBytes($imageFile, $image)

    DiskPart @("create vdisk file=`"$vhd`" maximum=64 type=fixed") | Out-Null
    $drive = Attach
    Write-Host "VHD disk: $drive"

    $list = & $bin list --allow-loop | Out-String
    Write-Host $list
    if ($list -notmatch "(?m)^$drive\s.*can be written") { Fail "$drive not offered with --allow-loop" }
    if ($list -notmatch "holds the running system") { Fail "the Windows disk isn't marked as the system disk" }
    if ((& $bin list | Out-String) -notmatch "(?m)^$drive\s.*refused: it is a virtual disk") { Fail "$drive offered without --allow-loop" }
    Pass "the VHD is offered only with --allow-loop, and the Windows disk is refused"

    $r = Write-Image $imageFile $drive @()
    if (-not $r.ok) { Write-Host $r.out; Fail "writing the image failed" }
    $drive = Compare-WithImage "plain"
    Pass "image written (whole sectors, partial last one padded), verified, identical"

    $gz = Join-Path $work 'image.img.gz'
    $out = [IO.File]::Create($gz)
    $zip = New-Object IO.Compression.GZipStream($out, [IO.Compression.CompressionLevel]::Fastest)
    $zip.Write($image, 0, $image.Length); $zip.Dispose(); $out.Dispose()
    $r = Write-Image $gz $drive @()
    if (-not $r.ok) { Write-Host $r.out; Fail "writing the .gz image failed" }
    $drive = Compare-WithImage ".gz"
    Pass "a gzip image is decompressed while writing, verified, identical"

    $number = $drive.Substring(13)
    DiskPart @("select disk $number", "clean", "create partition primary", "format fs=fat32 quick label=BCTEST", "assign letter=Q") | Out-Null
    Start-Sleep -Seconds 2
    $r = Write-Image $imageFile $drive @()
    if ($r.ok) { Fail "wrote a disk with a mounted volume" }
    if ($r.out -notmatch "is mounted at") { Fail "unexpected refusal: $($r.out)" }
    Pass "a disk with a mounted volume (Q:) is refused: $($r.out.Trim())"

    $r = Write-Image $imageFile $drive @('--unmount')
    if (-not $r.ok) { Write-Host $r.out; Fail "--unmount didn't write the disk" }
    $drive = Compare-WithImage "--unmount"
    Pass "--unmount locks and dismounts the volume, then writes and verifies"

    Write-Host "all Windows tests passed"
} finally {
    try { DiskPart @("select vdisk file=`"$vhd`"", "detach vdisk") | Out-Null } catch {}
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
