#!/usr/bin/env python3
"""把静态二进制打包成 Docker 镜像 tar（无需 Docker daemon）。

产物为标准 `docker save` 格式（v1.2），可在任意装有 Docker 的机器上：

    docker load -i y2m.tar
    docker run -p 8000:8000 y2m:latest
"""

import argparse
import hashlib
import io
import json
import os
import tarfile

ROOT = os.path.dirname(os.path.abspath(__file__))
DEFAULT_BINARY = os.path.join(
    ROOT, "target", "x86_64-unknown-linux-musl", "release", "y2m"
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def make_layer(binary_path: str) -> bytes:
    with open(binary_path, "rb") as f:
        data = f.read()
    if not data:
        raise SystemExit(f"二进制文件为空或读取失败: {binary_path}")
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        info = tarfile.TarInfo(name="y2m")
        info.size = len(data)
        info.mode = 0o755
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        info.mtime = 0
        tar.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=DEFAULT_BINARY, help="静态二进制路径")
    parser.add_argument("--tag", default="y2m:latest", help="镜像标签")
    parser.add_argument("--port", default="8000", help="暴露端口")
    parser.add_argument("-o", "--output", default="y2m.tar", help="输出 tar 文件名")
    args = parser.parse_args()

    if not os.path.isfile(args.binary):
        raise SystemExit(f"找不到二进制文件: {args.binary}")

    layer = make_layer(args.binary)
    layer_digest = sha256(layer)

    config = {
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Env": [
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            ],
            "Cmd": ["/y2m"],
            "WorkingDir": "/",
            "ExposedPorts": {f"{args.port}/tcp": {}},
        },
        "created": "1970-01-01T00:00:00Z",
        "rootfs": {"type": "layers", "diff_ids": [f"sha256:{layer_digest}"]},
        "history": [
            {"created": "1970-01-01T00:00:00Z", "created_by": "y2m image package"}
        ],
    }
    config_bytes = json.dumps(config, separators=(",", ":")).encode()
    config_digest = sha256(config_bytes)

    manifest = [
        {
            "Config": f"{config_digest}.json",
            "RepoTags": [args.tag],
            "Layers": [f"{layer_digest}/layer.tar"],
        }
    ]
    manifest_bytes = json.dumps(manifest, separators=(",", ":")).encode()

    with tarfile.open(args.output, "w") as tar:
        def add(name: str, data: bytes, mode: int = 0o644) -> None:
            info = tarfile.TarInfo(name=name)
            info.size = len(data)
            info.mode = mode
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = 0
            tar.addfile(info, io.BytesIO(data))

        def add_dir(name: str) -> None:
            info = tarfile.TarInfo(name=name)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = 0
            tar.addfile(info)

        add("manifest.json", manifest_bytes)
        add(f"{config_digest}.json", config_bytes)
        add_dir(f"{layer_digest}/")
        add(f"{layer_digest}/layer.tar", layer)

    size = os.path.getsize(args.output)
    print(f"已生成: {args.output} ({size} bytes)")
    print(f"镜像标签: {args.tag}")
    print()
    print("使用方式:")
    print(f"  docker load -i {args.output}")
    print(f"  docker run -p {args.port}:{args.port} {args.tag}")


if __name__ == "__main__":
    main()
