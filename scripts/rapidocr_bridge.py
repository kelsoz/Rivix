"""复用现有 RapidOCR 环境：从标准输入读 PNG，向标准输出写 JSON 文本。"""

import contextlib
import json
import sys
import os
import struct
import threading


_engine = None


def recognize(data):
    """返回识别文字，日志走 stderr，避免混入结果协议。"""
    global _engine
    with contextlib.redirect_stdout(sys.stderr):
        from rapidocr_onnxruntime import RapidOCR

        if _engine is None:
            # 本机 1/2/4/8 线程实测后采用四线程，降低小截图的线程池调度开销。
            _engine = RapidOCR(intra_op_num_threads=min(4, os.cpu_count() or 1), inter_op_num_threads=1)
        result, _ = _engine(data)
    return "\n".join(text.strip() for _, text, _ in result or [] if text.strip())


def read_exact(stream, count):
    """管道可能分段到达，拒绝截断数据。"""
    chunks = bytearray()
    while len(chunks) < count:
        chunk = stream.read(count - len(chunks))
        if not chunk:
            raise EOFError("OCR input truncated")
        chunks.extend(chunk)
    return bytes(chunks)


def worker():
    """每次请求一个尺寸受限的 RGBA 帧；EOF 或空闲时退出并释放模型内存。"""
    stream = sys.stdin.buffer
    while True:
        idle = threading.Timer(180, lambda: os._exit(0))
        idle.daemon = True
        idle.start()
        try:
            header = stream.read(4)
        finally:
            idle.cancel()
        if not header:
            return
        if len(header) != 4:
            raise EOFError("OCR header truncated")
        size = struct.unpack("<I", header)[0]
        if not 8 < size <= 256 * 1024 * 1024:
            raise ValueError("OCR request too large or empty")
        data = read_exact(stream, size)
        image = None
        try:
            import numpy as np
            width, height = struct.unpack("<II", data[:8])
            if width == 0 or height == 0 or width * height * 4 != len(data) - 8:
                raise ValueError("OCR dimensions do not match pixels")
            # RapidOCR 的 ndarray 输入为 BGR，明确转换，避免彩色文字的通道错位。
            image = np.frombuffer(data, dtype=np.uint8, offset=8).reshape(height, width, 4)
            text = recognize(image[:, :, :3][:, :, ::-1].copy())
            response = {"text": text}
        except Exception as error:
            response = {"error": str(error)}
        # 空闲时只保留模型，及时释放上一张截图的像素。
        image = None
        del data
        print(json.dumps(response, ensure_ascii=False, allow_nan=False), flush=True)


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")
    if "--worker" in sys.argv:
        worker()
    else:
        print(json.dumps(recognize(sys.stdin.buffer.read()), ensure_ascii=False, allow_nan=False))
