[CmdletBinding()]
param([switch]$WithOllama, [switch]$NoLaunch, [switch]$Help)

$ErrorActionPreference = 'Stop'

function Show-Help {
    @'
Usage: install.ps1 [-WithOllama] [-NoLaunch] [-Help]

Install LocalFlow on Windows. Piped installs use $HOME\.localflow\app.

Options:
  -WithOllama  Install and start Ollama
  -NoLaunch    Do not open the setup wizard after installation
  -Help        Show this help

Environment:
  LOCALFLOW_WITH_OLLAMA=1  Same as -WithOllama
  LOCALFLOW_NO_LAUNCH=1    Same as -NoLaunch
  LOCALFLOW_HOME           Install checkout location
  LOCALFLOW_REPO_URL       Git repository URL or local path
  LOCALFLOW_REF            Git ref to check out after cloning or pulling
'@ | Write-Host
}

if ($Help) {
    Show-Help
    return
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "LocalFlow's installer supports Windows only."
}

$WithOllama = $WithOllama -or ($env:LOCALFLOW_WITH_OLLAMA -eq '1')
$NoLaunch = $NoLaunch -or ($env:LOCALFLOW_NO_LAUNCH -eq '1')

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $false)][string[]]$ArgumentList = @()
    )

    & $FilePath @ArgumentList 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath $($ArgumentList -join ' ') failed with exit code $LASTEXITCODE."
    }
}

function Set-GitRef {
    param(
        [Parameter(Mandatory = $true)][string]$GitPath,
        [Parameter(Mandatory = $true)][string]$RepositoryPath,
        [Parameter(Mandatory = $true)][string]$RequestedRef
    )

    Invoke-Checked -FilePath $GitPath -ArgumentList @('-C', $RepositoryPath, 'fetch', 'origin')
    $refObject = "$RequestedRef^{commit}"
    & $GitPath -C $RepositoryPath rev-parse --verify --quiet $refObject *> $null
    if ($LASTEXITCODE -ne 0) {
        Invoke-Checked -FilePath $GitPath -ArgumentList @(
            '-C', $RepositoryPath, 'fetch', 'origin', $RequestedRef
        )
    }
    Invoke-Checked -FilePath $GitPath -ArgumentList @(
        '-C', $RepositoryPath, 'checkout', $RequestedRef
    )
}

function Replace-RepositoryFiles {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    if (Test-Path -LiteralPath $Destination) {
        $destinationItems = @(Get-ChildItem -LiteralPath $Destination -Force)
        if ($destinationItems.Count -gt 0) {
            $projectFile = Join-Path $Destination 'pyproject.toml'
            $isLocalFlowInstall = $false
            if (Test-Path -LiteralPath $projectFile -PathType Leaf) {
                $isLocalFlowInstall = [bool](Select-String -LiteralPath $projectFile `
                    -Pattern '^\s*name\s*=\s*"localflow"\s*(?:#.*)?$' -Quiet)
            }
            if (-not $isLocalFlowInstall) {
                throw "Install destination exists but is not a LocalFlow install: $Destination"
            }
        }
    } else {
        New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    }
    Get-ChildItem -LiteralPath $Destination -Force |
        Where-Object { $_.Name -ne '.venv' } |
        Remove-Item -Recurse -Force
    Get-ChildItem -LiteralPath $Source -Force | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $Destination -Recurse -Force
    }
}

$scriptRepo = $null
if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) {
    $candidateRepo = $PSScriptRoot
    if ((Test-Path -LiteralPath (Join-Path $candidateRepo 'pyproject.toml')) -and
        (Test-Path -LiteralPath (Join-Path $candidateRepo '.git'))) {
        $scriptRepo = $candidateRepo
    }
}

$repo = $null
$gitCommand = Get-Command git -ErrorAction SilentlyContinue
$repoUrl = $env:LOCALFLOW_REPO_URL
if ([string]::IsNullOrWhiteSpace($repoUrl)) {
    $repoUrl = 'https://github.com/CortexRND/localflow.git'
}
$ref = $env:LOCALFLOW_REF

if ($scriptRepo) {
    $repo = $scriptRepo
    if (-not [string]::IsNullOrWhiteSpace($ref)) {
        if (-not $gitCommand) {
            throw 'git is required to check out LOCALFLOW_REF from a checkout.'
        }
        Set-GitRef -GitPath $gitCommand.Source -RepositoryPath $repo -RequestedRef $ref
    }
} else {
    $repo = $env:LOCALFLOW_HOME
    if ([string]::IsNullOrWhiteSpace($repo)) {
        $repo = Join-Path $HOME '.localflow\app'
    }

    if ($gitCommand) {
        if (Test-Path -LiteralPath (Join-Path $repo '.git')) {
            if ([string]::IsNullOrWhiteSpace($ref)) {
                Invoke-Checked -FilePath $gitCommand.Source -ArgumentList @('-C', $repo, 'pull', '--ff-only')
            } else {
                Set-GitRef -GitPath $gitCommand.Source -RepositoryPath $repo -RequestedRef $ref
            }
        } else {
            $parent = Split-Path -Parent $repo
            if (-not (Test-Path -LiteralPath $parent)) {
                New-Item -ItemType Directory -Path $parent -Force | Out-Null
            }
            $temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("localflow-" + [guid]::NewGuid().ToString('N'))
            New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
            try {
                $clonePath = Join-Path $temporaryRoot 'checkout'
                Invoke-Checked -FilePath $gitCommand.Source -ArgumentList @('clone', $repoUrl, $clonePath)
                if (-not [string]::IsNullOrWhiteSpace($ref)) {
                    Set-GitRef -GitPath $gitCommand.Source -RepositoryPath $clonePath -RequestedRef $ref
                }
                Replace-RepositoryFiles -Source $clonePath -Destination $repo
            } finally {
                Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
            }
        }

    } else {
        $archiveRef = $ref
        if ([string]::IsNullOrWhiteSpace($archiveRef)) {
            $archiveRef = 'main'
        }
        $encodedRef = [Uri]::EscapeDataString($archiveRef)
        $archiveUrl = "https://github.com/CortexRND/localflow/archive/$encodedRef.zip"
        $temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("localflow-" + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
        try {
            $archivePath = Join-Path $temporaryRoot 'localflow.zip'
            $extractPath = Join-Path $temporaryRoot 'extract'
            Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath -UseBasicParsing
            Expand-Archive -LiteralPath $archivePath -DestinationPath $extractPath
            $sourceRepo = Get-ChildItem -LiteralPath $extractPath -Directory |
                Select-Object -First 1
            if (-not $sourceRepo) {
                throw "The LocalFlow archive was empty: $archiveUrl"
            }
            Replace-RepositoryFiles -Source $sourceRepo.FullName -Destination $repo
        } finally {
            Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

if (-not (Test-Path -LiteralPath (Join-Path $repo 'pyproject.toml'))) {
    throw "LocalFlow source was not found in $repo."
}

$pythonCommand = $null
$pythonPrefix = @()
$pythonCandidates = @(
    @{ Command = 'py'; Prefix = @('-3.13') },
    @{ Command = 'py'; Prefix = @('-3.12') },
    @{ Command = 'py'; Prefix = @('-3.11') },
    @{ Command = 'python'; Prefix = @() }
)
foreach ($candidate in $pythonCandidates) {
    if (-not (Get-Command $candidate.Command -ErrorAction SilentlyContinue)) {
        continue
    }
    $probeArguments = @($candidate.Prefix) + @(
        '-c',
        'import sys, tkinter; sys.exit(0 if sys.version_info >= (3, 11) else 1)'
    )
    & $candidate.Command @probeArguments 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) {
        $pythonCommand = $candidate.Command
        $pythonPrefix = @($candidate.Prefix)
        break
    }
}

$uvPath = $null
$useUv = $false
if (-not $pythonCommand) {
    $uvCommand = Get-Command uv -ErrorAction SilentlyContinue
    if (-not $uvCommand) {
        Invoke-RestMethod https://astral.sh/uv/install.ps1 | Invoke-Expression
        $uvCommand = Get-Command uv -ErrorAction SilentlyContinue
    }
    if ($uvCommand) {
        $uvPath = $uvCommand.Source
    } else {
        $uvCandidates = @(
            (Join-Path $HOME '.local\bin\uv.exe'),
            (Join-Path $HOME '.cargo\bin\uv.exe')
        )
        foreach ($candidatePath in $uvCandidates) {
            if (Test-Path -LiteralPath $candidatePath) {
                $uvPath = $candidatePath
                break
            }
        }
    }
    if (-not $uvPath) {
        throw 'uv installation failed or uv is not on PATH.'
    }
    Invoke-Checked -FilePath $uvPath -ArgumentList @('python', 'install', '3.12')
    $useUv = $true
}

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    $winget = Get-Command winget -ErrorAction SilentlyContinue
    if ($winget) {
        try {
            & $winget.Source install -e --id Gyan.FFmpeg --accept-source-agreements --accept-package-agreements
            if ($LASTEXITCODE -ne 0) {
                Write-Warning "winget could not install ffmpeg (exit code $LASTEXITCODE); continuing."
            }
        } catch {
            Write-Warning "winget could not install ffmpeg: $_"
        }
    }
}

Push-Location $repo
try {
    if ($useUv) {
        Invoke-Checked -FilePath $uvPath -ArgumentList @('venv', '--python', '3.12', '.venv')
    } else {
        $venvArguments = @($pythonPrefix) + @('-m', 'venv', '.venv')
        Invoke-Checked -FilePath $pythonCommand -ArgumentList $venvArguments
    }

    $venvPython = Join-Path $repo '.venv\Scripts\python.exe'
    & $venvPython -c 'import tkinter' 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw 'Python 3.11+ with tkinter is required.'
    }
    if ($useUv) {
        Invoke-Checked -FilePath $uvPath -ArgumentList @('pip', 'install', '--python', $venvPython, '--upgrade', 'pip')
        Invoke-Checked -FilePath $uvPath -ArgumentList @('pip', 'install', '--python', $venvPython, '-e', '.')
    } else {
        Invoke-Checked -FilePath $venvPython -ArgumentList @('-m', 'pip', 'install', '--upgrade', 'pip')
        Invoke-Checked -FilePath $venvPython -ArgumentList @('-m', 'pip', 'install', '-e', '.')
    }
} finally {
    Pop-Location
}

if ($WithOllama) {
    $ollama = Get-Command ollama -ErrorAction SilentlyContinue
    $ollamaPath = $null
    if ($ollama) {
        $ollamaPath = $ollama.Source
    }
    if (-not $ollama) {
        $winget = Get-Command winget -ErrorAction SilentlyContinue
        if (-not $winget) {
            throw 'Ollama is missing and winget is not available to install it.'
        }
        Invoke-Checked -FilePath $winget.Source -ArgumentList @(
            'install', '-e', '--id', 'Ollama.Ollama',
            '--accept-source-agreements', '--accept-package-agreements'
        )
        $ollama = Get-Command ollama -ErrorAction SilentlyContinue
        if ($ollama) {
            $ollamaPath = $ollama.Source
        } else {
            $ollamaCandidates = @(
                (Join-Path $env:LOCALAPPDATA 'Programs\Ollama\ollama.exe'),
                (Join-Path $env:ProgramFiles 'Ollama\ollama.exe')
            )
            foreach ($candidatePath in $ollamaCandidates) {
                if (Test-Path -LiteralPath $candidatePath) {
                    $ollamaPath = $candidatePath
                    break
                }
            }
        }
        if (-not $ollamaPath) {
            throw 'Ollama was installed but its executable could not be found.'
        }
    }

    $ollamaReady = $false
    try {
        $null = Invoke-RestMethod -Uri 'http://127.0.0.1:11434/api/tags' -TimeoutSec 2
        $ollamaReady = $true
    } catch {
        $ollamaReady = $false
    }
    $ollamaProcesses = Get-Process -Name 'ollama' -ErrorAction SilentlyContinue
    if (-not $ollamaReady -and -not $ollamaProcesses) {
        Start-Process -FilePath $ollamaPath -ArgumentList 'serve' -WindowStyle Hidden
    }
    $ollamaStopwatch = [Diagnostics.Stopwatch]::StartNew()
    while ($ollamaStopwatch.Elapsed.TotalSeconds -lt 30) {
        try {
            $null = Invoke-RestMethod -Uri 'http://127.0.0.1:11434/api/tags' -TimeoutSec 1
            $ollamaReady = $true
            break
        } catch {
            $ollamaReady = $false
        }
        if ($ollamaStopwatch.Elapsed.TotalSeconds -lt 30) {
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $ollamaReady) {
        throw 'Ollama did not become ready within 30 seconds.'
    }
}

$programs = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\LocalFlow'
New-Item -ItemType Directory -Path $programs -Force | Out-Null
$shell = New-Object -ComObject WScript.Shell
$localFlowPath = Join-Path $repo '.venv\Scripts\localflow.exe'
$localFlowShortcut = $shell.CreateShortcut((Join-Path $programs 'LocalFlow.lnk'))
$localFlowShortcut.TargetPath = $localFlowPath
$localFlowShortcut.WorkingDirectory = $repo
$localFlowShortcut.Save()

$setupPath = Join-Path $repo '.venv\Scripts\pythonw.exe'
$setupShortcut = $shell.CreateShortcut((Join-Path $programs 'LocalFlow Setup.lnk'))
$setupShortcut.TargetPath = $setupPath
$setupShortcut.Arguments = '-m localflow.setup_window'
$setupShortcut.WorkingDirectory = $repo
$setupShortcut.Save()

$scriptsPath = Join-Path $repo '.venv\Scripts'
Write-Host "To run lf in a new PowerShell window, add '$scriptsPath' to your PATH."

if ($NoLaunch) {
    Write-Host 'Skipping setup launch.'
} else {
    $lfPath = Join-Path $repo '.venv\Scripts\lf.exe'
    & $lfPath setup
    if ($LASTEXITCODE -ne 0) {
        throw "lf setup failed with exit code $LASTEXITCODE."
    }
}
