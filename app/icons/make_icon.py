# Draws the HdrCast app icon (a display with a play mark on graphite, with one bright highlight) and writes the sizes Tauri wants.
import sys, math
from PIL import Image, ImageDraw, ImageFilter

out = sys.argv[1]
S = 1024
SS = 2          # supersample
N = S * SS

def lerp(a, b, t): return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))

# diagonal gradient: near black -> graphite
stops = [(0.0, (14, 14, 16)), (0.6, (44, 44, 48)), (1.0, (92, 92, 98))]
grad = Image.new("RGB", (N, N))
px = grad.load()
for y in range(N):
    for x in range(0, N):
        t = (x * 0.35 + (N - y) * 0.65) / N
        t = min(1.0, max(0.0, t))
        for i in range(len(stops) - 1):
            if t <= stops[i + 1][0]:
                a, b = stops[i], stops[i + 1]
                px[x, y] = lerp(a[1], b[1], (t - a[0]) / (b[0] - a[0]))
                break

# a soft bright bloom in the upper right: the "HDR" highlight
bloom = Image.new("L", (N, N), 0)
ImageDraw.Draw(bloom).ellipse([N * 0.52, -N * 0.18, N * 1.12, N * 0.42], fill=105)
bloom = bloom.filter(ImageFilter.GaussianBlur(N * 0.09))
white = Image.new("RGB", (N, N), (255, 255, 255))
grad = Image.composite(white, grad, bloom)

# macOS-style rounded square (superellipse), with the standard margin so it sits right in the Dock
mask = Image.new("L", (N, N), 0)
md = ImageDraw.Draw(mask)
m = N * 0.098
r = (N - 2 * m) / 2
cx = cy = N / 2
pts = []
for i in range(720):
    a = 2 * math.pi * i / 720
    c, s = math.cos(a), math.sin(a)
    e = 2 / 5.0
    pts.append((cx + r * (abs(c) ** e) * (1 if c >= 0 else -1), cy + r * (abs(s) ** e) * (1 if s >= 0 else -1)))
md.polygon(pts, fill=255)

icon = Image.new("RGBA", (N, N), (0, 0, 0, 0))
icon.paste(grad, (0, 0), mask)

# glyph: a display outline with a play triangle
g = Image.new("RGBA", (N, N), (0, 0, 0, 0))
d = ImageDraw.Draw(g)
W = (255, 255, 255, 255)
x0, y0, x1, y1 = N * 0.255, N * 0.30, N * 0.745, N * 0.635
lw = int(N * 0.038)
d.rounded_rectangle([x0, y0, x1, y1], radius=N * 0.055, outline=W, width=lw)
# stand
d.rounded_rectangle([N * 0.40, N * 0.70, N * 0.60, N * 0.70 + lw], radius=lw / 2, fill=W)
d.rectangle([N * 0.5 - lw / 2, y1, N * 0.5 + lw / 2, N * 0.70 + lw / 2], fill=W)
# play triangle (rounded by drawing with a joined stroke)
tcx, tcy, tr = N * 0.508, (y0 + y1) / 2, N * 0.075
tri = [(tcx - tr * 0.62, tcy - tr), (tcx + tr * 0.95, tcy), (tcx - tr * 0.62, tcy + tr)]
d.polygon(tri, fill=W)
d.line(tri + [tri[0]], fill=W, width=int(N * 0.018), joint="curve")

shadow = g.split()[3].filter(ImageFilter.GaussianBlur(N * 0.012))
sh = Image.new("RGBA", (N, N), (0, 0, 0, 0))
sh.putalpha(shadow.point(lambda v: int(v * 0.35)))
icon = Image.alpha_composite(icon, Image.composite(sh, Image.new("RGBA", (N, N), (0, 0, 0, 0)), mask))
icon = Image.alpha_composite(icon, g)
icon = icon.resize((S, S), Image.LANCZOS)

icon.save(f"{out}/icon.png")
for name, size in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)]:
    icon.resize((size, size), Image.LANCZOS).save(f"{out}/{name}")
icon.save(f"{out}/icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
icon.save(f"{out}/icon.icns")
print("ok")
