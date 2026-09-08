# make-assets.ps1 - render the Echo icon into the tiles the UWP manifest needs.
#
#   .\make-assets.ps1
#
# ---- This is the ANDROID icon, redrawn -----------------------------------
#
# The Android client's launcher icon is an adaptive icon built from vector
# drawables, with no PNG anywhere to copy:
#
#   android/app/src/main/res/drawable/ic_launcher_background.xml
#   android/app/src/main/res/drawable/ic_launcher_foreground.xml
#
# So the geometry is reproduced here instead. Both files use a 108x108 viewport:
#
#   background  a flat #0B0C10 field
#   foreground  two open #00E5FF arcs radiating LEFT of a solid #00E5FF node,
#               all inside a 36-unit radius of the 54,54 centre
#
#     M 34,74 A 40,40 0 0,1 34,34     stroke 6, round caps
#     M 46,66 A 28,28 0 0,1 46,42     stroke 6, round caps
#     node at 64,54 r 8               filled
#
# An SVG elliptical arc is given by its endpoints; GDI+ wants a bounding box
# plus angles, so the centres are solved from the chord. For the outer arc:
# the chord runs 34,74 -> 34,34, so it is vertical, 40 long, and its midpoint
# is 34,54. With r=40 the centre sits sqrt(40^2 - 20^2) = 34.641 away along the
# perpendicular. sweep-flag 1 with the node to the RIGHT puts the centre right
# as well, at 68.641,54 - which makes the arc bulge left, toward nothing, which
# is what "radiating" means here. Angles then run 150 deg to 210 deg. Same
# construction for the inner arc: centre 71.298,54, r 28, 154.6 deg for 50.8.
#
# GDI+ measures angles clockwise from +x with y increasing downward, which is
# also SVG's convention, so the numbers transfer unchanged.
#
# If the Android icon is ever redrawn, redraw it here too - there is no shared
# source, and that is the cost of Android using vectors that UWP cannot read.

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$OutDir = Join-Path $PSScriptRoot "EchoXbox\Assets"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$Background = [System.Drawing.Color]::FromArgb(255, 11, 12, 16)    # #0B0C10
$Accent     = [System.Drawing.Color]::FromArgb(255, 0, 229, 255)   # #00E5FF

# name, width, height. Exactly what Package.appxmanifest references.
$tiles = @(
    @{ Name = "Square44x44Logo.png";   W = 44;   H = 44   },
    @{ Name = "Square150x150Logo.png"; W = 150;  H = 150  },
    @{ Name = "Wide310x150Logo.png";   W = 310;  H = 150  },
    @{ Name = "StoreLogo.png";         W = 50;   H = 50   },
    @{ Name = "SplashScreen.png";      W = 620;  H = 300  }
)

function Draw-EchoMark {
    param(
        [System.Drawing.Graphics] $g,
        [int] $Width,
        [int] $Height
    )

    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear($Background)

    # Map the 108-unit viewport onto the tile, centred, fitting the short edge.
    # On a wide tile that leaves the mark centred with the field either side,
    # which is what the splash and wide tile should look like anyway.
    $scale = [Math]::Min($Width, $Height) / 108.0
    $state = $g.Save()
    $g.TranslateTransform(($Width - 108.0 * $scale) / 2.0, ($Height - 108.0 * $scale) / 2.0)
    $g.ScaleTransform($scale, $scale)

    $pen = New-Object System.Drawing.Pen($Accent, 6.0)
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap   = [System.Drawing.Drawing2D.LineCap]::Round
    $brush = New-Object System.Drawing.SolidBrush($Accent)

    try {
        # Outer arc: centre 68.641,54  r 40  ->  box 28.641,14  80x80
        $g.DrawArc($pen, 28.641, 14.0, 80.0, 80.0, 150.0, 60.0)
        # Inner arc: centre 71.298,54  r 28  ->  box 43.298,26  56x56
        $g.DrawArc($pen, 43.298, 26.0, 56.0, 56.0, 154.6, 50.8)
        # The node: 64,54 r 8
        $g.FillEllipse($brush, 56.0, 46.0, 16.0, 16.0)
    } finally {
        $pen.Dispose()
        $brush.Dispose()
        $g.Restore($state)
    }
}

foreach ($tile in $tiles) {
    $bmp = New-Object System.Drawing.Bitmap($tile.W, $tile.H)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        Draw-EchoMark -g $g -Width $tile.W -Height $tile.H
    } finally {
        $g.Dispose()
    }
    $path = Join-Path $OutDir $tile.Name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host ("  {0,-24} {1}x{2}" -f $tile.Name, $tile.W, $tile.H) -ForegroundColor Green
}

Write-Host "`nWrote 5 tiles to $OutDir (Echo mark, matching the Android icon)"
