#!/usr/bin/env python3
"""把静态二进制打包成 Docker 镜像 tar（无需 Docker daemon）。

产物为标准 `docker save` 格式（v1.2），可在任意装有 Docker 的机器上：

    docker load -i y2m-<version>.tar
    docker run -p 8000:8000 y2m:<version>
"""

import argparse
from datetime import datetime, timezone
import hashlib
import io
import json
import os
import re
import tarfile

ROOT = os.path.dirname(os.path.abspath(__file__))
DEFAULT_BINARY = os.path.join(
    ROOT, "target", "x86_64-unknown-linux-musl", "release", "y2m"
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def make_layer(binary_path: str, mtime: int) -> bytes:
    with open(binary_path, "rb") as f:
        data = f.read()
    if not data:
        raise SystemExit(f"二进制文件为空或读取失败: {binary_path}")
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        # scratch 镜像没有标准临时目录，SQLite 在线备份需要它创建目标文件。
        info = tarfile.TarInfo(name="tmp/")
        info.type = tarfile.DIRTYPE
        info.mode = 0o1777
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        info.mtime = mtime
        tar.addfile(info)
        info = tarfile.TarInfo(name="y2m")
        info.size = len(data)
        info.mode = 0o755
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        info.mtime = mtime
        tar.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=DEFAULT_BINARY, help="静态二进制路径")
    parser.add_argument("--tag", default=None, help="镜像标签，默认使用 y2m:<版本号>")
    parser.add_argument("--port", default="8000", help="暴露端口")
    parser.add_argument("-o", "--output", default=None, help="输出 tar 文件名，默认使用 y2m-<版本号>.tar")
    args = parser.parse_args()

    version = input("请输入版本号: ").strip()
    if not version:
        raise SystemExit("版本号不能为空")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", version):
        raise SystemExit("版本号只能包含字母、数字、点、下划线和连字符，长度最多 128 个字符")

    created_at = datetime.now(timezone.utc).replace(microsecond=0)
    created = created_at.isoformat().replace("+00:00", "Z")
    mtime = int(created_at.timestamp())
    tag = args.tag or f"y2m:{version}"
    output = args.output or f"y2m-{version}.tar"

    if not os.path.isfile(args.binary):
        raise SystemExit(f"找不到二进制文件: {args.binary}")

    layer = make_layer(args.binary, mtime)
    layer_digest = sha256(layer)

    config = {
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Env": [
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            ],
            "Labels": {
                "org.opencontainers.image.version": version,
                "org.opencontainers.image.created": created,
            },
            "Cmd": ["/y2m"],
            "WorkingDir": "/",
            "ExposedPorts": {f"{args.port}/tcp": {}},
        },
        "created": created,
        "rootfs": {"type": "layers", "diff_ids": [f"sha256:{layer_digest}"]},
        "history": [
            {"created": created, "created_by": f"y2m image package {version}"}
        ],
    }
    config_bytes = json.dumps(config, separators=(",", ":")).encode()
    config_digest = sha256(config_bytes)

    manifest = [
        {
            "Config": f"{config_digest}.json",
            "RepoTags": [tag],
            "Layers": [f"{layer_digest}/layer.tar"],
        }
    ]
    manifest_bytes = json.dumps(manifest, separators=(",", ":")).encode()

    with tarfile.open(output, "w") as tar:
        def add(name: str, data: bytes, mode: int = 0o644) -> None:
            info = tarfile.TarInfo(name=name)
            info.size = len(data)
            info.mode = mode
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = mtime
            tar.addfile(info, io.BytesIO(data))

        def add_dir(name: str) -> None:
            info = tarfile.TarInfo(name=name)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = mtime
            tar.addfile(info)

        add("manifest.json", manifest_bytes)
        add(f"{config_digest}.json", config_bytes)
        add_dir(f"{layer_digest}/")
        add(f"{layer_digest}/layer.tar", layer)

    size = os.path.getsize(output)
    print(f"已生成: {output} ({size} bytes)")
    print(f"镜像标签: {tag}")
    print(f"版本号: {version}")
    print(f"创建时间 (UTC): {created}")
    print()
    print("使用方式:")
    print(f"  docker load -i {output}")
    print(f"  docker run -p {args.port}:{args.port} {tag}")


if __name__ == "__main__":
    main()
