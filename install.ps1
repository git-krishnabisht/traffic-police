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
    # the binary comes in this many parts at once (Save-InParts says why)
    $partCount = 8
    # Windows PowerShell 5.1 downloads slowly while it draws a progress bar, may not offer TLS 1.2,
    # which GitHub requires, and opens two connections to a server at most
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    if ([Net.ServicePointManager]::DefaultConnectionLimit -lt $partCount) { [Net.ServicePointManager]::DefaultConnectionLimit = $partCount }

    $repo = 'git-krishnabisht/traffic-police'
    $source = "https://github.com/$repo#install"
    $asset = 'traffic-police-windows-x86_64.exe'

    # One line per step: a mark, the step's name in a column, and what it found. The marks are
    # made from their code points, so this file stays ASCII however it is fetched; where the font
    # may lack them (the old console window), they are + and x.
    $fancy = $env:WT_SESSION -or $env:TERM_PROGRAM -eq 'vscode'
    $ok = if ($fancy) { [string][char]0x2713 } else { '+' }
    $bad = if ($fancy) { [string][char]0x2717 } else { 'x' }
    function Step([string] $mark, [ConsoleColor] $color, [string] $name, [string] $detail) {
        Write-Host '  ' -NoNewline
        Write-Host $mark -ForegroundColor $color -NoNewline
        Write-Host (' {0,-10} ' -f $name) -NoNewline
        Write-Host $detail
    }
    function Ok([string] $name, [string] $detail) { Step $ok Green $name $detail }
    function Warn([string] $name, [string] $detail) { Step '!' Yellow $name $detail }
    function Note([string] $text) { Write-Host ('               ' + $text) -ForegroundColor DarkGray }
    function Fail([string] $message) {
        Step $bad Red 'Failed' $message
        throw "traffic-police installer: $message"
    }
    # the download's progress: a spinner and a bar
    $frames = if ($fancy) { 0x280B, 0x2819, 0x2839, 0x2838, 0x283C, 0x2834, 0x2826, 0x2827, 0x2807, 0x280F | ForEach-Object { [string][char]$_ } } else { '|', '/', '-', '\' }
    $full = if ($fancy) { [string][char]0x2501 } else { '#' }
    $empty = if ($fancy) { [string][char]0x2500 } else { '-' }

    # one part, in a runspace of its own: a range request, and when it ends early, one for the
    # rest, three in all
    $partScript = {
        param([string] $url, [string] $path, [long] $from, [long] $to, [hashtable] $state, [int] $k)
        $buffer = [byte[]]::new(65536)
        $file = [IO.File]::Create($path)
        try {
            [long] $done = 0
            for ($try = 1; $from + $done -le $to; $try++) {
                if ($try -gt 3 -or $state.stop) { throw "part $k is incomplete" }
                try {
                    $request = [Net.HttpWebRequest]::Create($url)
                    $request.AddRange($from + $done, $to)
                    $request.Timeout = 30000
                    $request.ReadWriteTimeout = 60000
                    $state["request$k"] = $request
                    $response = $request.GetResponse()
                    $state["response$k"] = $response
                    try {
                        # a server that does not send parts sends the whole file
                        if ([int] $response.StatusCode -ne 206 -or $response.Headers['Content-Range'] -notlike "bytes $($from + $done)-*") {
                            $state.refused = $true
                            throw 'the server does not send parts'
                        }
                        $stream = $response.GetResponseStream()
                        while (($n = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                            $file.Write($buffer, 0, $n)
                            $done += $n
                            $state[$k] = $done
                        }
                    } finally {
                        $response.Close()
                    }
                } catch {
                    if ($state.refused) { throw }
                    [Threading.Thread]::Sleep(1000)
                }
            }
        } finally {
            $file.Close()
        }
    }
    # A large file comes in parts, each over its own connection: at times GitHub's release servers
    # give each connection from a network about 100 KB/s while the same network takes 20 MB/s from
    # elsewhere, and then eight parts at once arrive about six times as fast (measured on
    # 2026-10-11: 116 KB/s over one connection, 682 KB/s over eight; when one connection is fast,
    # parts cost nothing). Each part's size is checked, and the checksum after checks the whole.
    # When a part fails, or the server does not send parts, this throws, and the file comes over
    # one connection instead.
    function Save-InParts([string] $url, [string] $out, [long] $size) {
        $chunk = [long] [Math]::Ceiling($size / $partCount)
        $state = [hashtable]::Synchronized(@{})
        $jobs = @()
        $whole = $false
        $drawn = 0
        $pool = [RunspaceFactory]::CreateRunspacePool(1, $partCount, [Management.Automation.Runspaces.InitialSessionState]::CreateDefault2(), $Host)
        $pool.Open()
        try {
            for ($k = 0; $k -lt $partCount; $k++) {
                $from = $k * $chunk
                $to = [Math]::Min($from + $chunk, $size) - 1
                $ps = [PowerShell]::Create()
                $ps.RunspacePool = $pool
                [void] $ps.AddScript($partScript).AddArgument($url).AddArgument("$out.part$k").AddArgument($from).AddArgument($to).AddArgument($state).AddArgument($k)
                $jobs += [pscustomobject] @{ PS = $ps; Run = $ps.BeginInvoke(); Size = $to - $from + 1; Seen = 0L; Since = Get-Date }
            }
            # the parts so far, on a line drawn over in a console; a part that gets nothing for a
            # minute is cut off, and tries again
            $live = -not [Console]::IsOutputRedirected
            $begun = Get-Date
            $i = 0
            while (@($jobs | Where-Object { -not $_.Run.IsCompleted }).Count) {
                if ($state.refused) { throw 'the server does not send parts' }
                $now = Get-Date
                [long] $got = 0
                for ($k = 0; $k -lt $partCount; $k++) {
                    $job = $jobs[$k]
                    $n = [long] $state[$k]
                    $got += $n
                    if ($n -ne $job.Seen) {
                        $job.Seen = $n
                        $job.Since = $now
                    } elseif (-not $job.Run.IsCompleted -and ($now - $job.Since).TotalSeconds -ge 60) {
                        try { $state["request$k"].Abort() } catch { }
                        try { $state["response$k"].Close() } catch { }
                        $job.Since = $now
                    }
                }
                if ($live) {
                    $pct = [Math]::Min(100, [int] [Math]::Floor($got * 100 / $size))
                    $filled = [int] [Math]::Floor($pct / 5)
                    $rate = $got / [Math]::Max(1, ($now - $begun).TotalSeconds)
                    $speed = if ($rate -ge 1MB) { '{0:N1} MB/s' -f ($rate / 1MB) } else { '{0:N0} KB/s' -f ($rate / 1KB) }
                    $tail = ' {0,3}%  {1:N1} / {2:N1} MB  {3}   ' -f $pct, ($got / 1MB), ($size / 1MB), $speed
                    Write-Host "`r  " -NoNewline
                    Write-Host $frames[$i % $frames.Count] -ForegroundColor Cyan -NoNewline
                    Write-Host (' {0,-10} ' -f 'Download') -NoNewline
                    Write-Host (($full * $filled) + ($empty * (20 - $filled))) -ForegroundColor Cyan -NoNewline
                    Write-Host $tail -NoNewline
                    $drawn = 2 + 1 + 12 + 20 + $tail.Length
                    $i++
                }
                Start-Sleep -Milliseconds 100
            }
            foreach ($job in $jobs) { [void] $job.PS.EndInvoke($job.Run) }
            for ($k = 0; $k -lt $partCount; $k++) {
                if ((Get-Item -LiteralPath "$out.part$k").Length -ne $jobs[$k].Size) { throw "part $k has the wrong size" }
            }
            $joined = [IO.File]::Create($out)
            try {
                for ($k = 0; $k -lt $partCount; $k++) {
                    $in = [IO.File]::OpenRead("$out.part$k")
                    try { $in.CopyTo($joined) } finally { $in.Close() }
                }
            } finally {
                $joined.Close()
            }
            $whole = $true
        } finally {
            if ($drawn) { Write-Host ("`r" + (' ' * $drawn) + "`r") -NoNewline }
            # parts still running stop: cut off, they do not try again
            $state.stop = $true
            for ($k = 0; $k -lt $jobs.Count; $k++) {
                if (-not $jobs[$k].Run.IsCompleted) {
                    try { $state["request$k"].Abort() } catch { }
                    try { $state["response$k"].Close() } catch { }
                }
            }
            foreach ($job in $jobs) { $job.PS.Dispose() }
            $pool.Dispose()
            for ($k = 0; $k -lt $partCount; $k++) { Remove-Item -LiteralPath "$out.part$k" -Force -ErrorAction SilentlyContinue }
            if (-not $whole) { Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue }
        }
    }

    Write-Host ''
    Write-Host '  traffic-police installer'
    Write-Host ''

    if ($env:OS -ne 'Windows_NT') {
        Fail "this is the Windows installer; on macOS and Linux run: curl -fsSL https://raw.githubusercontent.com/$repo/master/install.sh | sh"
    }
    # a 32-bit PowerShell on 64-bit Windows finds the machine's own in PROCESSOR_ARCHITEW6432
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { Ok 'System' 'Windows on x86_64' }
        'ARM64' { Ok 'System' "Windows on ARM (the x86_64 build runs under Windows 11's emulation)" }
        default { Fail "there is no build for $arch Windows; build it from source: $source" }
    }

    $version = "$env:TRAFFIC_POLICE_VERSION".Trim()
    if ($version -eq '' -or $version -eq 'latest') {
        # the latest release's tag, from GitHub's redirect, so every file comes from the same one
        $version = $null
        try {
            $request = [Net.HttpWebRequest]::Create("https://github.com/$repo/releases/latest/download/$asset")
            $request.Method = 'HEAD'
            $request.AllowAutoRedirect = $false
            $request.Timeout = 30000
            $response = $request.GetResponse()
            $location = $response.Headers['Location']
            $response.Close()
            if ($location -match '/releases/download/([^/]+)/') { $version = $Matches[1] }
        } catch {
            Fail "could not reach GitHub ($($_.Exception.Message))"
        }
        if (-not $version) { Fail 'GitHub did not say which release is the latest' }
        Ok 'Release' "$version (the latest)"
    } else {
        if ($version -match '^\d+\.\d+\.\d+') { $version = "v$version" }
        if ($version -notmatch '^v\d+\.\d+\.\d+') { Fail "TRAFFIC_POLICE_VERSION takes a release like v0.4.0, not '$version'" }
        Ok 'Release' $version
    }
    $base = "https://github.com/$repo/releases/download/$version"
    $what = "release $version"

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('traffic-police-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $file = Join-Path $tmp $asset
        $sums = Join-Path $tmp 'SHA256SUMS.txt'
        # the binary's size, and whether its server sends parts of it
        $size = -1L
        $ranges = $false
        try {
            $request = [Net.HttpWebRequest]::Create("$base/$asset")
            $request.Method = 'HEAD'
            $request.Timeout = 30000
            $response = $request.GetResponse()
            $size = $response.ContentLength
            $ranges = $response.Headers['Accept-Ranges'] -eq 'bytes'
            $response.Close()
        } catch {
            # the download says what is wrong, if anything is
        }
        $started = Get-Date
        $inParts = $false
        if ($ranges -and $size -ge 1MB) {
            try {
                Save-InParts "$base/$asset" $file $size
                $inParts = $true
            } catch {
                # over one connection, then
            }
        }
        if (-not $inParts) {
            # PowerShell 7 draws its own progress bar for the download; Windows PowerShell 5.1 slows
            # a download down badly while it draws one, so there the line says it is under way
            if ($PSVersionTable.PSVersion.Major -ge 7) {
                $ProgressPreference = 'Continue'
            } else {
                Write-Host "  ...  Download   $asset (a minute or more on a slow connection)"
            }
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $file
            } catch {
                Fail "could not download $base/$asset ($($_.Exception.Message))"
            } finally {
                $ProgressPreference = 'SilentlyContinue'
            }
        }
        $mb = '{0:N1}' -f ((Get-Item -LiteralPath $file).Length / 1MB)
        $secs = [int]((Get-Date) - $started).TotalSeconds
        $took = if ($secs -ge 60) { '{0}m {1:D2}s' -f [int][Math]::Floor($secs / 60), ($secs % 60) } else { "${secs}s" }
        $over = if ($inParts) { " over $partCount connections" } else { '' }
        Ok 'Download' "$asset, $mb MB in $took$over"
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.txt" -OutFile $sums
        } catch {
            Fail "could not download $base/SHA256SUMS.txt ($($_.Exception.Message))"
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
        Ok 'Checksum' 'matches SHA256SUMS.txt'

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
        Ok 'Installed' "$installed in $dir"

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
            Ok 'adb' $adb
        } else {
            Warn 'adb' "not found: traffic-police needs it (Android's platform-tools) to reach a device"
            Note 'Android Studio has it, or the platform-tools from https://developer.android.com/tools/releases/platform-tools'
            Note '(traffic-police demo works without it)'
        }

        # PATH, for this user: the stored value as it is (%VARIABLES% stay unexpanded), plus the
        # folder; then a change of a variable through .NET tells Explorer, so new terminals see it
        $onPath = ($env:Path -split ';') -contains $dir
        $next = 'Try it: traffic-police demo'
        if ($onPath) {
            Ok 'PATH' "$dir is on it"
        } elseif ($env:TRAFFIC_POLICE_NO_MODIFY_PATH -eq '1') {
            Warn 'PATH' "$dir is not on it; add it to run traffic-police by its name"
        } else {
            $key = Get-Item -LiteralPath 'HKCU:\Environment'
            $userPath = [string] $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
            if (($userPath -split ';') -notcontains $dir) {
                $kept = $userPath.TrimEnd(';')
                $new = if ($kept) { "$kept;$dir" } else { $dir }
                Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name Path -Value $new -Type ExpandString
                [Environment]::SetEnvironmentVariable('TRAFFIC_POLICE_INSTALLER', '1', 'User')
                [Environment]::SetEnvironmentVariable('TRAFFIC_POLICE_INSTALLER', $null, 'User')
                Ok 'PATH' "added $dir to your PATH"
            } else {
                Ok 'PATH' "$dir is on your PATH (new terminals have it)"
            }
            $env:Path = "$env:Path;$dir"
            $next = 'Open a new terminal, then try it: traffic-police demo'
        }
        Write-Host ''
        Write-Host '  ' -NoNewline
        Write-Host 'Done.' -ForegroundColor Green -NoNewline
        Write-Host " $next"
        Write-Host ''
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
