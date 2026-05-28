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

  # System libraries needed at runtime by eframe (wgpu backend) and Blitz
  # (Vello/wgpu). Cargo does not link these statically; LD_LIBRARY_PATH lets
  # the binary find them inside the devshell.
  env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
    with pkgs;
    [
      libxkbcommon
      vulkan-loader
      libGL
      wayland
      xorg.libX11
      xorg.libXcursor
      xorg.libXi
      xorg.libXrandr
      xorg.libxcb
      fontconfig
      freetype
    ]
  );

  git-hooks.hooks = {
    clippy = {
      enable = true;
      settings.allFeatures = true;
    };
    rustfmt.enable = true;
  };
}
