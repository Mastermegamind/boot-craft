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

function Invoke-DiskPart([string[]] $lines) {
    $script = Join-Path $work 'diskpart.txt'
    Set-Content $script $lines -Encoding ascii
    $out = diskpart.exe /s $script | Out-String
    if ($LASTEXITCODE -ne 0) { Write-Host $out; throw "diskpart failed" }
    $out
}

function Attach {
    Invoke-DiskPart @("select vdisk file=`"$vhd`"", "attach vdisk") | Out-Null
    Start-Sleep -Seconds 2
    $disk = Get-Disk | Where-Object { $_.Location -eq $vhd -or $_.BusType -eq 'File Backed Virtual' } | Select-Object -Last 1
    if (-not $disk) { throw "the VHD didn't show up as a disk" }
    "PhysicalDrive$($disk.Number)"
}

# A fixed VHD is the disk's bytes followed by a footer: detach it and compare
# its start with the image, without trusting boot-craft's own read-back.
function Compare-WithImage([string] $what) {
    Invoke-DiskPart @("select vdisk file=`"$vhd`"", "detach vdisk") | Out-Null
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

    Invoke-DiskPart @("create vdisk file=`"$vhd`" maximum=64 type=fixed") | Out-Null
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
    Invoke-DiskPart @("select disk $number", "clean", "create partition primary", "format fs=fat32 quick label=BCTEST", "assign letter=Q") | Out-Null
    Start-Sleep -Seconds 2
    $r = Write-Image $imageFile $drive @()
    if ($r.ok) { Fail "wrote a disk with a mounted volume" }
    if ($r.out -notmatch "is mounted at") { Fail "unexpected refusal: $($r.out)" }
    Pass "a disk with a mounted volume (Q:) is refused: $($r.out.Trim())"

    $r = Write-Image $imageFile $drive @('--unmount')
    if (-not $r.ok) { Write-Host $r.out; Fail "--unmount didn't write the disk" }
    $drive = Compare-WithImage "--unmount"
    Pass "--unmount locks and dismounts the volume, then writes and verifies"

    # An NTFS Windows install drive from the fixture ISO: Windows itself must
    # mount it, find nothing wrong with chkdsk, and read every file as the ISO has it.
    $iso = Join-Path $work 'windows.iso'
    $in = [IO.File]::OpenRead((Resolve-Path 'crates\bc-core\tests\fixtures\windows.iso.gz'))
    $unzip = New-Object IO.Compression.GZipStream($in, [IO.Compression.CompressionMode]::Decompress)
    $outIso = [IO.File]::Create($iso)
    $unzip.CopyTo($outIso); $outIso.Dispose(); $unzip.Dispose(); $in.Dispose()
    $loader = Join-Path $work 'uefi-ntfs.img'
    Invoke-WebRequest -UseBasicParsing -OutFile $loader 'https://raw.githubusercontent.com/pbatard/rufus/master/res/uefi/uefi-ntfs.img'
    if ((Get-FileHash $loader -Algorithm SHA256).Hash -ne '72683FA1250EEEA772D3399277B434D4E55BA8DD0DC926E52D817E701FC2EB9E') { Fail "uefi-ntfs.img changed upstream" }
    $number = [int]$drive.Substring(13)
    Invoke-DiskPart @("select disk $number", "clean") | Out-Null
    $r = Write-Image $iso $drive @('--filesystem', 'ntfs', '--uefi-ntfs', $loader)
    if (-not $r.ok) { Write-Host $r.out; Fail "writing an NTFS Windows drive failed" }
    Write-Host $r.out
    Invoke-DiskPart @("rescan") | Out-Null
    Start-Sleep -Seconds 3
    $part = Get-Partition -DiskNumber $number | Where-Object { $_.PartitionNumber -eq 1 }
    if (-not $part.DriveLetter -or $part.DriveLetter -eq [char]0) {
        $part | Add-PartitionAccessPath -AssignDriveLetter
        Start-Sleep -Seconds 2
        $part = Get-Partition -DiskNumber $number | Where-Object { $_.PartitionNumber -eq 1 }
    }
    $letter = "$($part.DriveLetter):"
    $vol = Get-Volume -DriveLetter $part.DriveLetter
    if ($vol.FileSystem -ne 'NTFS') { Fail "Windows sees $($vol.FileSystem), not NTFS, on $letter" }
    if ($vol.FileSystemLabel -ne 'CCCOMA_X64FRE_EN-US_DV9') { Fail "label: $($vol.FileSystemLabel)" }
    $check = chkdsk.exe $letter | Out-String
    $code = $LASTEXITCODE
    Write-Host $check
    if ($code -ne 0 -or $check -notmatch 'found no problems') { Fail "chkdsk found problems on the NTFS drive (exit code $code)" }
    Pass "Windows mounts the NTFS drive and chkdsk finds no problems"

    $mounted = Mount-DiskImage -ImagePath $iso -PassThru
    try {
        $isoRoot = "$(($mounted | Get-Volume).DriveLetter):\"
        $want = Get-ChildItem -Recurse -File $isoRoot | ForEach-Object { "$($_.FullName.Substring($isoRoot.Length)) $((Get-FileHash $_.FullName).Hash)" } | Sort-Object
        $got = Get-ChildItem -Recurse -File "$letter\" | ForEach-Object { "$($_.FullName.Substring(3)) $((Get-FileHash $_.FullName).Hash)" } | Sort-Object
        if (Compare-Object $want $got) { Compare-Object $want $got | Out-String | Write-Host; Fail "Windows reads different files from the NTFS drive" }
        Pass "Windows reads all $($got.Count) files on the NTFS drive as the ISO has them"
    } finally { Dismount-DiskImage -ImagePath $iso | Out-Null }

    Write-Host "all Windows tests passed"
} finally {
    try { Invoke-DiskPart @("select vdisk file=`"$vhd`"", "detach vdisk") | Out-Null } catch {}
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
