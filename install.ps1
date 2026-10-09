# Installs traffic-police, the terminal network inspector for Android apps, on Windows:
#
#   irm https://raw.githubusercontent.com/git-krishnabisht/traffic-police/master/install.ps1 | iex
#
# It downloads the Windows binary from a GitHub release (the latest, unless TRAFFIC_POLICE_VERSION
# names one, like v0.4.0), checks it against the release's SHA256SUMS.txt, puts it in
# %LOCALAPPDATA%\Programs\traffic-police (no administrator rights) and adds that folder to your
# PATH. Run it again to update. adb is not installed: traffic-police uses the one you have
# (Android Studio's SDK or PATH), and the script says where it found it or how to get it.
#
# TRAFFIC_POLICE_INSTALL_DIR names another folder, and TRAFFIC_POLICE_NO_MODIFY_PATH=1 leaves PATH
# alone. Works in Windows PowerShell 5.1 and PowerShell 7. On macOS and Linux, install.sh does the
# same.

# one script block, so that nothing it sets stays in the session that ran it
& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell 5.1 downloads slowly while it draws a progress bar, and may not offer
    # TLS 1.2, which GitHub requires
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'git-krishnabisht/traffic-police'
    $source = "https://github.com/$repo#install"
    $asset = 'traffic-police-windows-x86_64.exe'

    function Fail([string] $message) {
        throw "traffic-police installer: $message"
    }

    if ($env:OS -ne 'Windows_NT') {
        Fail "this is the Windows installer; on macOS and Linux run: curl -fsSL https://raw.githubusercontent.com/$repo/master/install.sh | sh"
    }
    # a 32-bit PowerShell on 64-bit Windows finds the machine's own in PROCESSOR_ARCHITEW6432
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Write-Host "Windows on ARM: the x86_64 build runs under Windows 11's emulation." }
        default { Fail "there is no build for $arch Windows; build it from source: $source" }
    }

    $version = "$env:TRAFFIC_POLICE_VERSION".Trim()
    if ($version -eq '' -or $version -eq 'latest') {
        $base = "https://github.com/$repo/releases/latest/download"
        $what = 'the latest release'
    } else {
        if ($version -match '^\d+\.\d+\.\d+') { $version = "v$version" }
        if ($version -notmatch '^v\d+\.\d+\.\d+') { Fail "TRAFFIC_POLICE_VERSION takes a release like v0.4.0, not '$version'" }
        $base = "https://github.com/$repo/releases/download/$version"
        $what = "release $version"
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('traffic-police-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $asset from $what..."
        $file = Join-Path $tmp $asset
        $sums = Join-Path $tmp 'SHA256SUMS.txt'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $file
            Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.txt" -OutFile $sums
        } catch {
            Fail "could not download from $base ($($_.Exception.Message))"
        }
        $want = $null
        foreach ($line in Get-Content -LiteralPath $sums) {
            $parts = $line.Trim() -split '\s+'
            if ($parts.Count -ge 2 -and ($parts[1] -eq $asset -or $parts[1] -eq "*$asset")) {
                $want = $parts[0].ToLowerInvariant()
                break
            }
        }
        if (-not $want) { Fail "SHA256SUMS.txt of $what has no line for $asset" }
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant()
        if ($got -ne $want) { Fail "the download does not match its checksum (expected $want, got $got); nothing was installed" }

        $dir = if ($env:TRAFFIC_POLICE_INSTALL_DIR) { $env:TRAFFIC_POLICE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\traffic-police' }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $exe = Join-Path $dir 'traffic-police.exe'
        # a running traffic-police.exe cannot be replaced, but it can be renamed: the old file
        # moves aside, and the next install removes it
        Get-ChildItem -LiteralPath $dir -Filter 'traffic-police.exe.old-*' -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $exe) {
            try {
                Remove-Item -LiteralPath $exe -Force
            } catch {
                Move-Item -LiteralPath $exe -Destination ("$exe.old-" + [Guid]::NewGuid().ToString('N').Substring(0, 8)) -Force
            }
        }
        Copy-Item -LiteralPath $file -Destination $exe -Force
        Unblock-File -LiteralPath $exe -ErrorAction SilentlyContinue
        $installed = (& $exe --version | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { Fail "$exe was installed but does not run here: $installed" }
        Write-Host "Installed $installed in $exe"

        # adb: where traffic-police looks for it (the SDK's platform-tools first, then PATH)
        $adb = $null
        foreach ($sdk in @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT, (Join-Path $env:LOCALAPPDATA 'Android\Sdk'))) {
            if ($sdk -and (Test-Path -LiteralPath (Join-Path $sdk 'platform-tools\adb.exe'))) {
                $adb = Join-Path $sdk 'platform-tools\adb.exe'
                break
            }
        }
        if (-not $adb) {
            $command = Get-Command adb.exe -ErrorAction SilentlyContinue
            if ($command) { $adb = $command.Source }
        }
        if ($adb) {
            Write-Host "adb: $adb"
        } else {
            Write-Host "adb was not found. traffic-police needs it (Android's platform-tools) to reach a device:"
            Write-Host '  Android Studio has it, or the platform-tools from https://developer.android.com/tools/releases/platform-tools'
            Write-Host '  (traffic-police demo works without it)'
        }

        # PATH, for this user: the stored value as it is (%VARIABLES% stay unexpanded), plus the
        # folder; then a change of a variable through .NET tells Explorer, so new terminals see it
        $onPath = ($env:Path -split ';') -contains $dir
        if ($onPath) {
            Write-Host 'Try it: traffic-police demo'
        } elseif ($env:TRAFFIC_POLICE_NO_MODIFY_PATH -eq '1') {
            Write-Host "$dir is not on your PATH; add it to run traffic-police by its name."
        } else {
            $key = Get-Item -LiteralPath 'HKCU:\Environment'
            $userPath = [string] $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
            if (($userPath -split ';') -notcontains $dir) {
                $kept = $userPath.TrimEnd(';')
                $new = if ($kept) { "$kept;$dir" } else { $dir }
                Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name Path -Value $new -Type ExpandString
                [Environment]::SetEnvironmentVariable('TRAFFIC_POLICE_INSTALLER', '1', 'User')
                [Environment]::SetEnvironmentVariable('TRAFFIC_POLICE_INSTALLER', $null, 'User')
                Write-Host "Added $dir to your PATH."
            }
            $env:Path = "$env:Path;$dir"
            Write-Host 'Open a new terminal, then try it: traffic-police demo'
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
