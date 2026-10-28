"""内嵌二进制解析单测:平台 wheel 内嵌路径的存在性分支。

正向路径(真实平台 wheel 内嵌二进制)由 CI 的 wheel 安装冒烟覆盖:
仓库源码树不内嵌二进制,单测用临时目录伪造包结构验证两个分支。
"""

import os
import stat
from pathlib import Path

from zhuye.agent import _bundled_binary


def _make_pkg_dir(tmp_path: Path, with_bin: bool) -> Path:
    """伪造 zhuye 包目录;with_bin 时创建可执行的内嵌二进制占位。"""
    pkg_dir = tmp_path / "zhuye"
    pkg_dir.mkdir()
    if with_bin:
        exe = "zhuye.exe" if os.name == "nt" else "zhuye"
        bin_file = pkg_dir / "bin" / exe
        bin_file.parent.mkdir()
        bin_file.write_bytes(b"\x7fELF-fake")
        bin_file.chmod(0o755)
    return pkg_dir


def test_bundled_binary_平台wheel内嵌时返回路径(tmp_path: Path) -> None:
    pkg_dir = _make_pkg_dir(tmp_path, with_bin=True)
    resolved = _bundled_binary(pkg_dir)
    assert resolved is not None
    assert Path(resolved).is_file()
    assert Path(resolved).stat().st_mode & stat.S_IXUSR


def test_bundled_binary_源码运行无内嵌时返回_none(tmp_path: Path) -> None:
    pkg_dir = _make_pkg_dir(tmp_path, with_bin=False)
    assert _bundled_binary(pkg_dir) is None


def test_bundled_binary_仓库源码树默认无内嵌() -> None:
    """仓库内运行(生产入口不传 package_dir)应回退 None。

    若该断言失败,说明有人把二进制提交进了源码树 —— 平台二进制只能
    由 CI 打进 wheel,不得入库。
    """
    assert _bundled_binary() is None
