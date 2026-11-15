# Installs the VB-Audio Virtual Cable driver on a runner with no audio hardware, so Windows
# has an audio endpoint and the GS Wavetable Synth opens (`synth-tests`).
#
# The download is pinned by SHA-256 and verified on every run, whether it came from the vendor
# or from the workflow's cache (keyed by the same hash); a mismatch fails the step. A vendor
# that swaps the zip therefore breaks only a cold cache.
# To bump the pin: change $Url and $Sha256 here and the cache key in ci.yml's `clippy-windows`.
# The device node is created through SetupAPI (what `devcon install` does), so no helper
# binary is downloaded.
$ErrorActionPreference = 'Stop'

$Url = 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip'
$Sha256 = 'bc44e8974191a5a2427d6c979e2dab08dbb09925f3f435dfa2f0ba3e45b6eb90'
$HardwareId = 'VBAudioVACWDM'
$InfName = 'vbMmeCable64_win10.inf'

$work = Join-Path $env:RUNNER_TEMP 'vbcable'
New-Item -ItemType Directory -Force -Path $work | Out-Null
# The workflow caches this directory under the pinned hash.
$cacheDir = Join-Path $env:RUNNER_TEMP 'vbcable-cache'
New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
$zip = Join-Path $cacheDir 'vbcable.zip'
if (Test-Path $zip) {
    Write-Host 'using the cached driver pack'
} else {
    Invoke-WebRequest -Uri $Url -OutFile $zip -TimeoutSec 120 -MaximumRetryCount 3 -RetryIntervalSec 5
}
$got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
if ($got -ne $Sha256) {
    Remove-Item -Force $zip
    throw "VB-Cable zip hash mismatch: got $got, pinned $Sha256"
}
$dir = Join-Path $work 'pack'
Expand-Archive -LiteralPath $zip -DestinationPath $dir
$inf = Join-Path $dir $InfName

# The win10 package's catalog is signed by Microsoft (WHQL), so no publisher is added to the
# trust store; a catalog that does not verify fails the install.
$sig = Get-AuthenticodeSignature -FilePath (Join-Path $dir 'vbaudio_cable64_win10.cat')
Write-Host "catalog signature: $($sig.Status) by $($sig.SignerCertificate.Subject)"
if ($sig.Status -ne 'Valid') { throw 'VB-Cable catalog signature is not valid' }

pnputil /add-driver $inf /install
if ($LASTEXITCODE -ne 0) { throw "pnputil /add-driver failed with exit code $LASTEXITCODE" }

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class RootDevice {
    [StructLayout(LayoutKind.Sequential)]
    struct SP_DEVINFO_DATA { public int cbSize; public Guid ClassGuid; public int DevInst; public IntPtr Reserved; }
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SetupDiGetINFClassW(string inf, ref Guid guid, System.Text.StringBuilder name, int size, out int req);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid guid, IntPtr parent);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SetupDiCreateDeviceInfoW(IntPtr set, string name, ref Guid guid, string desc, IntPtr hwnd, int flags, ref SP_DEVINFO_DATA data);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr set, ref SP_DEVINFO_DATA data, int prop, byte[] buf, int size);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiCallClassInstaller(int fn, IntPtr set, ref SP_DEVINFO_DATA data);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("newdev.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool UpdateDriverForPlugAndPlayDevicesW(IntPtr hwnd, string hwid, string inf, int flags, out bool reboot);

    static void Check(bool ok, string what) {
        if (!ok) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), what);
    }

    public static bool Install(string inf, string hwid) {
        Guid guid = Guid.Empty;
        var cls = new System.Text.StringBuilder(32);
        int req;
        Check(SetupDiGetINFClassW(inf, ref guid, cls, cls.Capacity, out req), "SetupDiGetINFClass");
        IntPtr set = SetupDiCreateDeviceInfoList(ref guid, IntPtr.Zero);
        if (set == new IntPtr(-1)) Check(false, "SetupDiCreateDeviceInfoList");
        try {
            var data = new SP_DEVINFO_DATA();
            data.cbSize = Marshal.SizeOf(typeof(SP_DEVINFO_DATA));
            Check(SetupDiCreateDeviceInfoW(set, cls.ToString(), ref guid, null, IntPtr.Zero, 1 /*DICD_GENERATE_ID*/, ref data), "SetupDiCreateDeviceInfo");
            byte[] multi = System.Text.Encoding.Unicode.GetBytes(hwid + "\0\0");
            Check(SetupDiSetDeviceRegistryPropertyW(set, ref data, 1 /*SPDRP_HARDWAREID*/, multi, multi.Length), "SetupDiSetDeviceRegistryProperty");
            Check(SetupDiCallClassInstaller(0x19 /*DIF_REGISTERDEVICE*/, set, ref data), "DIF_REGISTERDEVICE");
        } finally { SetupDiDestroyDeviceInfoList(set); }
        bool reboot;
        Check(UpdateDriverForPlugAndPlayDevicesW(IntPtr.Zero, hwid, inf, 1 /*INSTALLFLAG_FORCE*/, out reboot), "UpdateDriverForPlugAndPlayDevices");
        return reboot;
    }
}
'@
$reboot = [RootDevice]::Install($inf, $HardwareId)
# Nothing can reboot the runner mid-job; the endpoint wait below decides whether the driver works.
if ($reboot) { Write-Warning 'UpdateDriverForPlugAndPlayDevices asks for a reboot; waiting for the endpoint anyway' }

foreach ($s in 'AudioEndpointBuilder', 'Audiosrv') {
    Set-Service -Name $s -StartupType Automatic
    Start-Service -Name $s
}

# The endpoint appears a moment after the driver binds.
$deadline = (Get-Date).AddSeconds(60)
do {
    $ep = Get-PnpDevice -Class AudioEndpoint -Status OK -ErrorAction SilentlyContinue
    if ($ep) { break }
    Start-Sleep -Seconds 2
} while ((Get-Date) -lt $deadline)
Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue | Format-Table -AutoSize | Out-String | Write-Host
if (-not $ep) { throw 'no audio endpoint after installing the virtual cable' }
