<#
.SYNOPSIS
    Writes the third-party notices for the program as it is currently built.

.DESCRIPTION
    The released .exe contains other people's code: the libraries it is linked
    with and the Rust standard library. Their licences ask that the copyright
    notice and licence text travel with every copy, so each release ships this
    file next to the program.

    The list comes from Cargo itself (the versions locked in Cargo.lock), and
    every licence text is copied from the library's own published source, so
    the result always matches the build it was generated for.

    Fails, rather than guessing, if a library has no MIT licence text.

.EXAMPLE
    pwsh tools/third-party-notices.ps1 -Output target/release/THIRD-PARTY-NOTICES.txt
#>
param([string]$Output = 'THIRD-PARTY-NOTICES.txt')

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

function Invoke-Cargo([string[]]$Arguments) {
    $result = & cargo @Arguments
    if ($LASTEXITCODE -ne 0) { throw "cargo $($Arguments -join ' ') failed" }
    $result
}

# Strips the indentation and stray blank lines some projects publish their licence with.
function Format-LicenceText([string]$Text) {
    $lines = @(($Text -replace "`r", '') -split "`n" | ForEach-Object { $_.TrimEnd() })
    $indents = @($lines | Where-Object { $_ } | ForEach-Object { $_.Length - $_.TrimStart().Length })
    $indent = if ($indents.Count) { ($indents | Measure-Object -Minimum).Minimum } else { 0 }
    $lines = @($lines | ForEach-Object { if ($_.Length -ge $indent) { $_.Substring($indent) } else { $_ } })
    (($lines -join "`n").Trim("`n"))
}

# Only what is compiled into the program: no build scripts, no procedural macros.
$inProgram = @{}
foreach ($line in Invoke-Cargo @('tree', '--locked', '-e', 'normal,no-proc-macro', '--prefix', 'none', '--format', '{p}')) {
    if ($line -match '^(\S+) v(\S+)') { $inProgram["$($Matches[1]) $($Matches[2])"] = $true }
}
$metadata = (Invoke-Cargo @('metadata', '--locked', '--format-version', '1')) -join "`n" | ConvertFrom-Json
$own = $metadata.resolve.root

# Libraries that publish the very same licence text are listed together.
$groups = [ordered]@{}
foreach ($package in $metadata.packages | Sort-Object name, version) {
    if ($package.id -eq $own -or -not $inProgram.ContainsKey("$($package.name) $($package.version)")) { continue }
    $label = "$($package.name) $($package.version)"
    $folder = Split-Path -Parent $package.manifest_path
    $file = Get-ChildItem -LiteralPath $folder -File |
        Where-Object { $_.Name -match '^(?i)licen[sc]e-mit(\.md|\.txt)?$' } |
        Select-Object -First 1
    if (-not $file -or $package.license -notmatch '\bMIT\b') {
        throw "No MIT licence text found for $label (declared licence: $($package.license)). " +
            'Teach tools/third-party-notices.ps1 how to attribute it before releasing.'
    }
    $text = Format-LicenceText (Get-Content -LiteralPath $file.FullName -Raw)
    if (-not $groups.Contains($text)) { $groups[$text] = New-Object System.Collections.ArrayList }
    [void]$groups[$text].Add([pscustomobject]@{ Label = $label; Source = $package.repository })
}
if ($groups.Count -eq 0) { throw 'Found no libraries at all; refusing to write an empty notice.' }

$rule = '=' * 78
$thin = '-' * 78
$out = New-Object System.Collections.ArrayList
[void]$out.Add(@"
THIRD-PARTY NOTICES

StarStrings Updater is released under the MIT License (see the LICENSE file in
its repository). The program also contains the third-party software listed
below, which is compiled into the executable. Each component is used under the
MIT License; every licence is reproduced here as its authors published it.

This list was generated for the exact library versions of this build.
"@)
[void]$out.Add('')

foreach ($text in $groups.Keys) {
    $members = $groups[$text]
    [void]$out.Add($rule)
    # The names, wrapped so the file stays readable in a plain text viewer.
    $line = ''
    foreach ($label in $members | ForEach-Object { $_.Label }) {
        if ($line -and ($line.Length + $label.Length + 2) -gt 78) {
            [void]$out.Add("$line,")
            $line = $label
        } elseif ($line) {
            $line = "$line, $label"
        } else {
            $line = $label
        }
    }
    [void]$out.Add($line)
    foreach ($source in $members | ForEach-Object { $_.Source } | Where-Object { $_ } | Sort-Object -Unique) {
        [void]$out.Add("    $source")
    }
    [void]$out.Add($thin)
    [void]$out.Add($text)
    [void]$out.Add('')
}

[void]$out.Add($rule)
[void]$out.Add(@"
The Rust standard library
    https://github.com/rust-lang/rust
$thin
Copyright (c) The Rust Project Contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

The standard library itself incorporates code from other projects. The complete
list of their notices ships with every Rust toolchain, in the file
share/doc/rust/COPYRIGHT-library.html.
"@)

# Windows line endings, since this is read in Notepad next to a Windows program.
$content = ((($out -join "`n") -replace "`r", '') -replace "`n", "`r`n") + "`r`n"
$target = [IO.Path]::GetFullPath((Join-Path (Get-Location).Path $Output))
[IO.File]::WriteAllText($target, $content, (New-Object System.Text.UTF8Encoding $false))
"Wrote $target ($(@($groups.Values | ForEach-Object { $_ }).Count) libraries, $($groups.Count) distinct licence texts)"
