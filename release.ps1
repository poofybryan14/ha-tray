# release.ps1 - publie une nouvelle version de HaTray sur GitHub.
#
# Utilisation (depuis le dossier du projet) :
#   .\release.cmd 0.3.0 "Ajout de la couleur des lampes"
#
# Ce que fait le script :
#   1. change la version dans Cargo.toml ;
#   2. compile (si ca echoue, rien n'est publie) ;
#   3. enregistre tout dans Git ("commit") et cree le tag v0.3.0 ;
#   4. envoie sur GitHub : GitHub Actions compile et cree la release.
# (ASCII uniquement : PowerShell 5.1 lit mal les accents sans BOM.)

param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$Message = "Nouvelle version"
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$Version = $Version.TrimStart('v')
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version invalide '$Version' : utilisez le format 0.3.0" }
$tag = "v$Version"

# Le tag ne doit pas deja exister.
if (git tag --list $tag) { throw "Le tag $tag existe deja. Choisissez un numero de version plus grand." }

# 1. Version dans Cargo.toml (premiere occurrence = celle du package)
Write-Host "1/4 Version $Version dans Cargo.toml..." -ForegroundColor Cyan
$cargo = Get-Content Cargo.toml -Raw -Encoding UTF8
$regex = [regex]'(?m)^version = ".*"'
$cargo = $regex.Replace($cargo, "version = `"$Version`"", 1)
[System.IO.File]::WriteAllText("$PSScriptRoot\Cargo.toml", $cargo, (New-Object System.Text.UTF8Encoding($false)))

# 2. Compilation locale (met aussi a jour Cargo.lock)
Write-Host "2/4 Compilation..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) {
    git checkout -- Cargo.toml
    throw "La compilation a echoue : rien n'a ete publie, Cargo.toml est restaure."
}

# 3. Commit + tag
Write-Host "3/4 Commit et tag $tag..." -ForegroundColor Cyan
git add -A
git commit -m "Version ${Version} : $Message"
if ($LASTEXITCODE -ne 0) { throw "Le commit a echoue." }
git tag -a $tag -m $Message

# 4. Envoi
Write-Host "4/4 Envoi sur GitHub..." -ForegroundColor Cyan
git push
if ($LASTEXITCODE -ne 0) { throw "git push a echoue." }
git push origin $tag
if ($LASTEXITCODE -ne 0) { throw "L'envoi du tag a echoue." }

Write-Host ""
Write-Host "Termine. GitHub compile maintenant la version $Version (5 a 10 minutes)." -ForegroundColor Green
Write-Host "Suivi : onglet Actions de votre depot. La release apparait dans l'onglet Releases."
