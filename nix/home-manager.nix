{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.invocateur;
  inherit (lib) mkEnableOption mkIf mkOption types;
  multiplexerPackage =
    if cfg.multiplexer == "herdr"
    then cfg.herdrPackage
    else if cfg.multiplexer == "tmux"
    then cfg.tmuxPackage
    else cfg.zellijPackage;
  settings = {
    inherit (cfg) spells agent repositories multiplexer;
    workspace_name = cfg.workspaceName;
    tab_name = cfg.tabName;
    agent_args = cfg.agentArgs;
    acknowledgement_message = cfg.acknowledgementMessage;
    repos_directory = cfg.reposDirectory;
    discover_repositories = cfg.discoverRepositories;
    discover_github_repositories = cfg.discoverGithubRepositories;
    clone_missing = cfg.cloneMissing;
    poll_interval_seconds = cfg.pollInterval;
    initial_lookback_seconds = cfg.initialLookback;
    review_comments = cfg.reviewComments;
    herdr_session = cfg.herdrSession;
    tmux_session = cfg.tmuxSession;
    zellij_session = cfg.zellijSession;
    github_host = cfg.githubHost;
    gh_path = lib.getExe pkgs.gh;
    herdr_path =
      if cfg.multiplexer == "herdr"
      then lib.getExe multiplexerPackage
      else "herdr";
    tmux_path =
      if cfg.multiplexer == "tmux"
      then lib.getExe multiplexerPackage
      else "tmux";
    zellij_path =
      if cfg.multiplexer == "zellij"
      then lib.getExe multiplexerPackage
      else "zellij";
    git_path = lib.getExe pkgs.git;
    command_timeout_seconds = cfg.commandTimeout;
  };
  configFile = (pkgs.formats.json {}).generate "invocateur.json" settings;
in {
  options.services.invocateur = {
    enable = mkEnableOption "GitHub comment spells that start coding agents in a terminal multiplexer";
    multiplexer = mkOption {
      type = types.enum ["herdr" "tmux" "zellij"];
      default = "herdr";
      description = "Terminal multiplexer backend. The configured session must already be running.";
    };
    workspaceName = mkOption {
      type = types.str;
      default = "{repository}#{issue_number} {agent_name}";
      description = "Herdr workspace name template. Supports {repository}, {issue_number}, {agent}, {agent_name}, {comment_id}, {multiplexer}, and {session}.";
    };
    tabName = mkOption {
      type = types.str;
      default = "{agent_name}";
      description = "Herdr/Zellij tab or tmux window name template, with the same placeholders as workspaceName.";
    };
    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ../package.nix {};
      description = "The invocateur package.";
    };
    herdrPackage = mkOption {
      type = types.package;
      default = pkgs.herdr;
      description = "Herdr package, compatible with the running Herdr server.";
    };
    tmuxPackage = mkOption {
      type = types.package;
      default = pkgs.tmux;
      description = "tmux package used when multiplexer is tmux.";
    };
    zellijPackage = mkOption {
      type = types.package;
      default = pkgs.zellij;
      description = "Zellij package used when multiplexer is zellij. Requires support for new-tab --no-focus and an initial command.";
    };
    spells = mkOption {
      type = types.nonEmptyListOf types.str;
      default = ["summon"];
      example = ["summon" "/cast"];
      description = "Case-sensitive triggers, each on its own line outside Markdown quotes and code blocks.";
    };
    agent = mkOption {
      type = types.str;
      default = "codex";
      example = "claude";
      description = "Herdr agent kind, or the agent executable name/absolute path for tmux and Zellij.";
    };
    agentArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Native agent arguments. tmux and Zellij append the task as the final positional argument.";
    };
    acknowledgementMessage = mkOption {
      type = types.str;
      default = "🪄 Invocateur started {agent} for this task.\n\nTriggered by {comment_url}.";
      description = "Comment posted after successfully starting an agent. Supports {agent}, {comment_url}, {repository}, {issue_number}, {multiplexer}, and {session}.";
    };
    reposDirectory = mkOption {
      type = types.str;
      default = "${config.home.homeDirectory}/repos";
      description = "Directory to scan for GitHub checkouts, at most two directory levels deep.";
    };
    repositories = mkOption {
      type = types.attrsOf types.str;
      default = {};
      example = {"kosumi/nixpkgs" = "~/nixpkgs";};
      description = "Additional owner/repo to local checkout mappings. These override discovered paths.";
    };
    discoverRepositories = mkOption {
      type = types.bool;
      default = true;
      description = "Discover repositories from origin remotes under reposDirectory on every poll.";
    };
    discoverGithubRepositories = mkOption {
      type = types.bool;
      default = true;
      description = "Find your comments and reviews across GitHub, and watch their repositories. Missing checkouts use reposDirectory/owner/repo.";
    };
    cloneMissing = mkOption {
      type = types.bool;
      default = true;
      description = "Shallow clone missing checkouts when a spell triggers.";
    };
    pollInterval = mkOption {
      type = types.ints.positive;
      default = 60;
      description = "Seconds to wait between completed polls.";
    };
    initialLookback = mkOption {
      type = types.ints.unsigned;
      default = 0;
      description = "Seconds of comment history to consider when first watching a repository. Zero processes future updates only.";
    };
    reviewComments = mkOption {
      type = types.bool;
      default = true;
      description = "Also poll inline PR review comments, in addition to issue and PR conversation comments.";
    };
    herdrSession = mkOption {
      type = types.str;
      default = "default";
      description = "Existing Herdr session to target explicitly. Start it before using the service.";
    };
    tmuxSession = mkOption {
      type = types.str;
      default = "invocateur";
      description = "Existing tmux session for new detached agent windows. Use letters, digits, underscores, or hyphens.";
    };
    zellijSession = mkOption {
      type = types.str;
      default = "invocateur";
      description = "Existing Zellij session for new background agent tabs. Use letters, digits, underscores, or hyphens.";
    };
    githubHost = mkOption {
      type = types.str;
      default = "github.com";
      description = "GitHub hostname, authenticated through the local gh CLI.";
    };
    commandTimeout = mkOption {
      type = types.ints.positive;
      default = 90;
      description = "Timeout in seconds for each gh, git, or Herdr CLI invocation.";
    };
    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [pkgs.codex];
      description = "Agent executables available to the service. Herdr also needs its agent executable in the server's environment.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = pkgs.stdenv.hostPlatform.isLinux;
        message = "services.invocateur requires Linux systemd user services.";
      }
      {
        assertion = lib.all (spell: spell != "" && builtins.match "[^\r\n]+" spell != null) cfg.spells;
        message = "services.invocateur.spells must contain nonempty single-line strings.";
      }
    ];

    home.packages = [cfg.package multiplexerPackage];
    xdg.configFile."invocateur/config.json".source = configFile;

    systemd.user.services.invocateur = {
      Unit.Description = "Summon coding agents from GitHub comments";
      Service = {
        ExecStart = "${lib.getExe cfg.package} --config ${configFile} --state-dir %S/invocateur";
        Restart = "on-failure";
        RestartSec = 15;
        StateDirectory = "invocateur";
        UMask = "0077";
        WorkingDirectory = config.home.homeDirectory;
        Environment = [
          "XDG_CONFIG_HOME=${config.xdg.configHome}"
          "PATH=${lib.makeBinPath ([pkgs.gh pkgs.git multiplexerPackage] ++ cfg.extraPackages)}:${config.home.profileDirectory}/bin:${config.home.homeDirectory}/.local/bin:${config.home.homeDirectory}/.nix-profile/bin:/run/current-system/sw/bin"
        ];
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
