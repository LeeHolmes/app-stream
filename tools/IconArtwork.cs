using System;
using System.Drawing;
using System.Drawing.Imaging;

public static class IconArtwork
{
    public static void CleanResamplingNoise(Bitmap image)
    {
        for (int y = 0; y < image.Height; y++)
        for (int x = 0; x < image.Width; x++)
        {
            // Bicubic ringing can leave almost-invisible alpha outside the artwork.
            if (image.GetPixel(x, y).A <= 1)
                image.SetPixel(x, y, Color.Transparent);
        }
    }

    public static void ValidateTransparency(Bitmap image)
    {
        if (image.GetPixel(0, 0).A != 0
            || image.GetPixel(image.Width - 1, 0).A != 0
            || image.GetPixel(0, image.Height - 1).A != 0
            || image.GetPixel(image.Width - 1, image.Height - 1).A != 0)
            throw new InvalidOperationException("Icon corners must be fully transparent.");
        bool opaque = false, antialiased = false;
        for (int y = 0; y < image.Height; y++)
        for (int x = 0; x < image.Width; x++)
        {
            var pixel = image.GetPixel(x, y);
            opaque |= pixel.A == 255;
            antialiased |= pixel.A > 0 && pixel.A < 255;
            if (pixel.A > 8 && pixel.B - pixel.R < 30)
                throw new InvalidOperationException("Gray background remains in the icon.");
        }
        if (!opaque || !antialiased)
            throw new InvalidOperationException("Icon must preserve solid artwork and antialiased alpha edges.");
    }

    public static Bitmap RemoveBackground(Bitmap source)
    {
        var background = source.GetPixel(0, 0);
        var result = new Bitmap(source.Width, source.Height, PixelFormat.Format32bppArgb);
        var core = new bool[source.Width, source.Height];
        for (int y = 2; y < source.Height - 2; y++)
        for (int x = 2; x < source.Width - 2; x++)
        {
            bool inside = true;
            for (int dy = -2; dy <= 2 && inside; dy++)
            for (int dx = -2; dx <= 2; dx++)
            {
                var pixel = source.GetPixel(x + dx, y + dy);
                if (pixel.B - pixel.R < 100)
                {
                    inside = false;
                    break;
                }
            }
            core[x, y] = inside;
        }
        try
        {
            for (int y = 0; y < source.Height; y++)
            for (int x = 0; x < source.Width; x++)
            {
                var pixel = source.GetPixel(x, y);
                if (pixel.B - pixel.R <= 2)
                    continue;
                if (core[x, y])
                {
                    result.SetPixel(x, y, pixel);
                    continue;
                }

                // Estimate the original blue at antialiased edges from nearby solid artwork.
                int distance = int.MaxValue;
                Color foreground = Color.Empty;
                for (int dy = -16; dy <= 16; dy++)
                for (int dx = -16; dx <= 16; dx++)
                {
                    int nx = x + dx, ny = y + dy, d = dx * dx + dy * dy;
                    if (nx < 0 || nx >= source.Width || ny < 0 || ny >= source.Height
                        || d >= distance || !core[nx, ny])
                        continue;
                    distance = d;
                    foreground = source.GetPixel(nx, ny);
                }
                if (foreground.IsEmpty)
                    throw new InvalidOperationException("Cannot recover blue artwork near " + x + "," + y);

                double r = foreground.R - background.R;
                double g = foreground.G - background.G;
                double b = foreground.B - background.B;
                double alpha = ((pixel.R - background.R) * r
                    + (pixel.G - background.G) * g + (pixel.B - background.B) * b)
                    / (r * r + g * g + b * b);
                alpha = Math.Min(1, Math.Max(0, alpha));
                if (alpha < 1.0 / 255)
                    continue;
                result.SetPixel(x, y, Color.FromArgb(
                    Channel(alpha * 255),
                    Channel(background.R + (pixel.R - background.R) / alpha),
                    Channel(background.G + (pixel.G - background.G) / alpha),
                    Channel(background.B + (pixel.B - background.B) / alpha)));
            }
            return result;
        }
        catch
        {
            result.Dispose();
            throw;
        }
    }

    private static int Channel(double value)
    {
        return (int)Math.Round(Math.Min(255, Math.Max(0, value)));
    }
}
