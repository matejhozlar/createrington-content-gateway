import subprocess
import sys
import tempfile
from pathlib import Path

PACKAGE = Path(__file__).resolve().parents[1]
SPEC = PACKAGE.parents[1] / "openapi.json"
OUTPUT = PACKAGE / "src" / "content_gateway" / "generated.py"


def generate(into: Path) -> None:
    subprocess.run(
        [
            sys.executable,
            "-m",
            "datamodel_code_generator",
            "--input",
            str(SPEC),
            "--input-file-type",
            "openapi",
            "--output",
            str(into),
            "--output-model-type",
            "typing.TypedDict",
            "--target-python-version",
            "3.11",
            "--use-standard-collections",
            "--use-union-operator",
            "--enum-field-as-literal",
            "all",
            "--use-double-quotes",
            "--disable-timestamp",
            "--formatters",
            "ruff-format",
        ],
        check=True,
        cwd=PACKAGE,
    )
    into.write_bytes(into.read_bytes().replace(b"
", b"
"))


def main() -> int:
    if "--check" not in sys.argv[1:]:
        generate(OUTPUT)
        print(f"Wrote {OUTPUT.relative_to(PACKAGE)}")
        return 0
    with tempfile.TemporaryDirectory() as directory:
        fresh = Path(directory) / "generated.py"
        generate(fresh)
        if OUTPUT.read_bytes() == fresh.read_bytes():
            print(f"{OUTPUT.relative_to(PACKAGE)} is up to date")
            return 0
    print(
        f"{OUTPUT.relative_to(PACKAGE)} is stale, run scripts/generate.py and commit the result",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
