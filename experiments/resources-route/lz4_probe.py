"""Probe the D/ storage LZ4 files: pure-python LZ4 frame decode + content sniff."""
import glob, os, struct, sys, json

def lz4_block_decompress(src: bytes, uncompressed_size: int) -> bytes:
    out = bytearray()
    i = 0; n = len(src)
    while i < n:
        token = src[i]; i += 1
        lit_len = token >> 4
        if lit_len == 15:
            while True:
                b = src[i]; i += 1; lit_len += b
                if b != 255: break
        out += src[i:i+lit_len]; i += lit_len
        if i >= n: break
        offset = src[i] | (src[i+1] << 8); i += 2
        match_len = token & 0xF
        if match_len == 15:
            while True:
                b = src[i]; i += 1; match_len += b
                if b != 255: break
        match_len += 4
        start = len(out) - offset
        for j in range(match_len):
            out.append(out[start + j])
    assert len(out) == uncompressed_size, f"size mismatch {len(out)} != {uncompressed_size}"
    return bytes(out)

def lz4_frame_decompress(data: bytes) -> bytes:
    assert data[:4] == bytes.fromhex('04224D18'), 'not lz4 frame'
    i = 6  # magic(4) + FLG(1) + BD(1); assume no content size/dict id (check HC bit)
    flg = data[4]
    has_content_size = bool(flg & 0x08)
    has_dict_id = bool(flg & 0x04)
    if has_content_size:
        i += 8
    if has_dict_id:
        i += 4
    i += 1  # HC
    out = bytearray()
    while i < len(data):
        bsize = int.from_bytes(data[i:i+4], 'little'); i += 4
        if bsize == 0: break
        uncompressed = bool(bsize & 0x80000000)
        bsize &= 0x7FFFFFFF
        block = data[i:i+bsize]; i += bsize
        if uncompressed:
            out += block
        else:
            # need uncompressed size: LZ4 block has no size — decode without assert
            out += lz4_block_decompress(block, -1) if False else lz4_block_nosize(block)
    return bytes(out)

def lz4_block_nosize(src: bytes) -> bytes:
    out = bytearray(); i = 0; n = len(src)
    while i < n:
        token = src[i]; i += 1
        lit_len = token >> 4
        if lit_len == 15:
            while True:
                b = src[i]; i += 1; lit_len += b
                if b != 255: break
        out += src[i:i+lit_len]; i += lit_len
        if i >= n: break
        offset = src[i] | (src[i+1] << 8); i += 2
        match_len = token & 0xF
        if match_len == 15:
            while True:
                b = src[i]; i += 1; match_len += b
                if b != 255: break
        match_len += 4
        start = len(out) - offset
        if offset >= match_len:
            out += out[start:start+match_len]
        else:
            for j in range(match_len): out.append(out[start + j])
    return bytes(out)

C = '/Users/pixdane/Library/Containers/jp.co.bandainamcoent.BNEI0416/Data/Documents'
files = sorted(glob.glob(C + '/D/*/*'), key=os.path.getsize)
small = [f for f in files if open(f,'rb').read(4) == bytes.fromhex('04224D18')]
print('lz4 files:', len(small))
for f in small[:6]:
    data = open(f,'rb').read()
    try:
        raw = lz4_frame_decompress(data)
        head = raw[:120]
        print(f'{os.path.basename(f)} ({os.path.getsize(f)}B -> {len(raw)}B):', head[:100])
    except Exception as e:
        print(f'{os.path.basename(f)}: FAIL {e}')
