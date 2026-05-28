{
  pkgs,
  ...
}:

{
  packages = with pkgs; [
    bashInteractive

    # dev loop
    bacon
    go-task

    # cargo extensions
    cargo-flamegraph
    cargo-llvm-cov
    cargo-audit
    cargo-deny
    cargo-insta

    # mail testing (fixture sync + send stub target)
    isync # provides `mbsync`
    msmtp

    # misc
    perf
    llvmPackages.bintools
    prettier
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
    mold.enable = true;
    # cranelift.enable = true;  # debug-only, currently nightly; revisit after baseline
  };

  git-hooks.hooks = {
    clippy = {
      enable = true;
      settings.allFeatures = true;
    };
    rustfmt.enable = true;
  };
}
