"""由用户提供的 SVG 生成程序图标及深浅任务栏单色图标。"""
from pathlib import Path
import io
import fitz
from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1] / "assets"
PATHS = [
"M853.333333 128H170.666667c-25.6 0-42.666667 17.066667-42.666667 42.666667v384h85.333333V213.333333h597.333334v341.333334h85.333333V170.666667c0-25.6-17.066667-42.666667-42.666667-42.666667z",
"M725.333333 597.333333c-29.866667 0-59.733333 8.533333-85.333333 25.6l-55.466667-55.466666 162.133334-162.133334L686.933333 341.333333l-162.133333 162.133334L358.4 341.333333 298.666667 401.066667l162.133333 162.133333-64 64c-25.6-17.066667-59.733333-29.866667-98.133333-29.866667-93.866667 0-170.666667 76.8-170.666667 170.666667s76.8 170.666667 170.666667 170.666667 170.666667-76.8 170.666666-170.666667c0-25.6-4.266667-51.2-17.066666-72.533333l68.266666-68.266667 55.466667 55.466667c-17.066667 25.6-25.6 55.466667-25.6 85.333333 0 93.866667 76.8 170.666667 170.666667 170.666667s170.666667-76.8 170.666666-170.666667-72.533333-170.666667-166.4-170.666667zM298.666667 853.333333c-46.933333 0-85.333333-38.4-85.333334-85.333333s38.4-85.333333 85.333334-85.333333 85.333333 38.4 85.333333 85.333333-38.4 85.333333-85.333333 85.333333z m426.666666 0c-46.933333 0-85.333333-38.4-85.333333-85.333333s38.4-85.333333 85.333333-85.333333 85.333333 38.4 85.333334 85.333333-38.4 85.333333-85.333334 85.333333z"
]

def svg(color):
    return '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">' + ''.join(f'<path d="{p}" fill="{color}"/>' for p in PATHS) + '</svg>'

def icon(size, color, tile=False):
    with fitz.open(stream=svg(color).encode(), filetype='svg') as doc:
        pix = doc.get_page_pixmap(0, matrix=fitz.Matrix(size * 4 / 1024, size * 4 / 1024), alpha=True)
        glyph = Image.open(io.BytesIO(pix.tobytes('png'))).convert('RGBA')
    if tile:
        image = Image.new('RGBA', glyph.size)
        ImageDraw.Draw(image).rounded_rectangle((0, 0, image.width-1, image.height-1), radius=image.width//4, fill='#202020')
        image.alpha_composite(glyph)
    else:
        image = glyph
    return image.resize((size,size), Image.Resampling.LANCZOS)

if __name__ == '__main__':
    (ROOT / 'icon.svg').write_text(svg('#E8E8E8'), encoding='utf-8')
    for name,color in [('tray.png','#E8E8E8'),('tray-light.png','#242424')]:
        image = icon(32,color)
        assert image.getpixel((0,0))[3] == 0
        assert image.getbbox() is not None
        image.save(ROOT / name)
    image = icon(256,'#E8E8E8',tile=True)
    image.save(ROOT / 'app.ico', sizes=[(n,n) for n in [16,20,24,32,40,48,64,128,256]])
    image.save(ROOT / 'logo.png')
    assert len(Image.open(ROOT / 'app.ico').ico.sizes()) == 9
