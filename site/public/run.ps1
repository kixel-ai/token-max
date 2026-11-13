# Runs `token-max doctor` from the latest release without installing it:
#
#   irm https://max.kixel.ai/run.ps1 | iex
#   & ([scriptblock]::Create((irm https://max.kixel.ai/run.ps1))) --since 2026-11-01
#
# The binary is checked against its release checksum, cached under
# $env:XDG_CACHE_HOME or ~\.cache, in token-max\run, and reused while it is the latest.
# Each release's binary stays there until deleted, so no run removes one another is about to start.
# TOKEN_MAX_RELEASES points at another copy of the releases, for tests.
#
# Run through iex, `exit` would close the user's window: errors throw, and doctor's exit code
# is left in $LASTEXITCODE.

& {
	$ErrorActionPreference = 'Stop'
	# Windows PowerShell's progress bar slows downloads many times over.
	$ProgressPreference = 'SilentlyContinue'
	$releases = if ($env:TOKEN_MAX_RELEASES) { $env:TOKEN_MAX_RELEASES } else { 'https://github.com/kixel-ai/token-max/releases' }

	if (-not [Environment]::Is64BitOperatingSystem) {
		throw 'token-max: there is no prebuilt token-max for 32-bit Windows; see https://github.com/kixel-ai/token-max#install'
	}
	# Windows on Arm runs the x86_64 build under emulation.
	$target = 'x86_64-pc-windows-msvc'

	# GitHub redirects /releases/latest to /releases/tag/<tag>, or to /releases when there is none.
	$request = [Net.WebRequest]::Create("$releases/latest")
	$request.Method = 'HEAD'
	$request.AllowAutoRedirect = $false
	$response = $request.GetResponse()
	$latest = $response.Headers['Location']
	$response.Close()
	$tag = if ($latest) { ($latest -split '/')[-1] } else { '' }
	if ($tag -notmatch '^v[0-9]') { throw "token-max: $releases has no release yet" }
	if ($tag -notmatch '^[0-9A-Za-z.+-]+$') { throw "token-max: unexpected release tag $tag" }

	$base = if ($env:XDG_CACHE_HOME) { $env:XDG_CACHE_HOME } else { Join-Path $HOME '.cache' }
	$cache = Join-Path (Join-Path $base 'token-max') 'run'
	$exe = Join-Path $cache "token-max-$tag-$target.exe"
	if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
		# Beside the cache, so the final move is a rename.
		$tmp = Join-Path $cache ('.download.' + [guid]::NewGuid())
		New-Item -ItemType Directory -Path $tmp | Out-Null
		try {
			$archive = "token-max-$target.zip"
			[Console]::Error.WriteLine("token-max: downloading $tag for $target")
			$zip = Join-Path $tmp $archive
			Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$tag/$archive" -OutFile $zip
			Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$tag/$archive.sha256" -OutFile "$zip.sha256"
			$expected = ([IO.File]::ReadAllText("$zip.sha256").Trim() -split '\s+')[0]
			if ($expected -notmatch '^[0-9a-fA-F]{64}$') { throw "token-max: $archive.sha256 holds no SHA-256" }
			# .NET rather than Get-FileHash and Expand-Archive: Windows PowerShell started from
			# PowerShell 7 inherits a PSModulePath whose script modules it cannot load.
			$stream = [IO.File]::OpenRead($zip)
			try {
				$actual = [BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($stream)) -replace '-', ''
			} finally {
				$stream.Dispose()
			}
			if ($actual -ne $expected) {
				throw "token-max: $archive does not match its checksum; nothing was run"
			}
			Add-Type -AssemblyName System.IO.Compression.FileSystem
			$unpacked = Join-Path $tmp 'unpacked'
			[IO.Compression.ZipFile]::ExtractToDirectory($zip, $unpacked)
			try {
				[IO.File]::Move((Join-Path $unpacked 'token-max.exe'), $exe)
			} catch {
				# Another run placed the same verified release first, and may be running it.
				if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw }
			}
		} finally {
			Remove-Item -Recurse -Force -LiteralPath $tmp
		}
	}

	# Under Stop, Windows PowerShell turns doctor's redirected stderr into a terminating error.
	$ErrorActionPreference = 'Continue'
	& $exe doctor @args
} @args
