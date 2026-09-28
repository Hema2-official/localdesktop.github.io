#!/bin/sh
# Run common development workflows inside the guest and report which work and how long they take:
# git, a Python venv with pip, npm, Go, Rust, C with make, and makepkg. Needs the network for the
# clone and package downloads; skips tools that aren't installed. Run it as a normal user, e.g.:
#   scripts/dev-shell.sh -u alarm scripts/guest/dev-tools-check.sh
# Prints one PASS/FAIL/SKIP line per workflow; the exit status is the number of failures.

failures=0
work=$(mktemp -d "${TMPDIR:-/tmp}/dev-tools.XXXXXX") || exit 1
log="$work/log"
echo "dev tools check as $(id -un) in $work"

# check TOOL COMMAND...: run COMMAND (a shell snippet) in a fresh directory, if TOOL is installed.
check() {
    name=$1
    shift
    if ! command -v "$name" > /dev/null 2>&1; then
        echo "SKIP  $name (not installed)"
        return
    fi
    mkdir -p "$work/$name" && cd "$work/$name" || return
    start=$(date +%s%N)
    if sh -c "$*" > "$log" 2>&1; then
        echo "PASS  $name ($(( ($(date +%s%N) - start) / 1000000 )) ms)"
    else
        echo "FAIL  $name: $(tail -3 "$log" | tr '\n' ' ')"
        failures=$((failures + 1))
    fi
    cd "$work" || exit 1
}

check git 'git init -q repo && cd repo && git config user.email t@t && git config user.name t &&
    echo hello > f && git add f && git commit -qm first && git status --short | wc -l | grep -qx 0 &&
    cd .. && git clone -q --depth 1 https://github.com/octocat/Hello-World.git clone &&
    git -C clone log --oneline | grep -q .'
check python3 'python3 -m venv venv && ./venv/bin/pip install -q six &&
    ./venv/bin/python -c "import six; print(six.__version__)"'
check npm 'npm init -y > /dev/null && npm install --silent --no-audit --no-fund ms &&
    node -e "console.log(require(\"ms\")(\"2 days\"))" | grep -qx 172800000'
check go 'go mod init example.com/hello > /dev/null 2>&1 &&
    printf "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"hello\") }\n" > main.go &&
    GOFLAGS=-mod=mod go build -o hello . && ./hello | grep -qx hello'
check cargo 'cargo new -q --offline hello && cd hello && cargo build -q --offline &&
    ./target/debug/hello | grep -qx "Hello, world!"'
check make 'printf "#include <stdio.h>\nint main(void) { puts(\"hello\"); return 0; }\n" > hello.c &&
    printf "hello: hello.c\n\tcc -O2 -o hello hello.c\n" > Makefile && make -s && ./hello | grep -qx hello'
check makepkg 'printf "pkgname=hello-check\npkgver=1\npkgrel=1\narch=(any)\npackage() { install -Dm755 /dev/null \"\$pkgdir/usr/bin/hello-check\"; }\n" > PKGBUILD &&
    makepkg -s --noconfirm > /dev/null && pkg=$(ls hello-check-1-1-any.pkg.tar.*) &&
    bsdtar -tf "$pkg" | grep -c proot-meta | grep -qx 0 &&
    bsdtar -tvf "$pkg" usr/bin/hello-check | grep -q "^-rwxr-xr-x"'

rm -rf "$work"
echo "$failures failure(s)"
exit "$failures"
