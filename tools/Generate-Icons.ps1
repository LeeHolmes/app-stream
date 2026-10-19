param(
    [string]$Source = (Join-Path $PSScriptRoot '..\assets\icon-source.png'),
    [string]$Destination = (Join-Path $PSScriptRoot '..\assets')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
if (-not ('IconArtwork' -as [type])) {
    Add-Type -Path (Join-Path $PSScriptRoot 'IconArtwork.cs') -ReferencedAssemblies @(
        [System.Drawing.Bitmap].Assembly.Location,
        [System.Drawing.Color].Assembly.Location
    )
}
$original = [System.Drawing.Bitmap]::new($Source)
$sourceImage = $null
try {
    $sourceImage = [IconArtwork]::RemoveBackground($original)
    [IconArtwork]::ValidateTransparency($sourceImage)
    $sourceImage.Save((Join-Path $Destination 'icon-transparent.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    $sizes = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
    $images = @()
    foreach ($size in $sizes) {
        $bitmap = [System.Drawing.Bitmap]::new($size, $size)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.Clear([System.Drawing.Color]::Transparent)
            $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
            $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
            $scale = [Math]::Min($size / $sourceImage.Width, $size / $sourceImage.Height)
            $width = [single]($sourceImage.Width * $scale)
            $height = [single]($sourceImage.Height * $scale)
            $graphics.DrawImage($sourceImage, [single](($size - $width) / 2),
                [single](($size - $height) / 2), $width, $height)
            $path = Join-Path $Destination "app-stream-$size.png"
            [IconArtwork]::CleanResamplingNoise($bitmap)
            [IconArtwork]::ValidateTransparency($bitmap)
            $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
            $images += ,([System.IO.File]::ReadAllBytes($path))
        }
        finally {
            $graphics.Dispose()
            $bitmap.Dispose()
        }
    }
    $stream = [System.IO.File]::Create((Join-Path $Destination 'app-stream.ico'))
    $writer = [System.IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$sizes.Count)
        $offset = 6 + 16 * $sizes.Count
        for ($i = 0; $i -lt $sizes.Count; $i++) {
            $dimension = if ($sizes[$i] -eq 256) { 0 } else { $sizes[$i] }
            $writer.Write([byte]$dimension)
            $writer.Write([byte]$dimension)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]32)
            $writer.Write([uint32]$images[$i].Length)
            $writer.Write([uint32]$offset)
            $offset += $images[$i].Length
        }
        foreach ($image in $images) {
            $writer.Write([byte[]]$image)
        }
    }
    finally {
        $writer.Dispose()
    }
}
finally {
    if ($null -ne $sourceImage) {
        $sourceImage.Dispose()
    }
    $original.Dispose()
}
