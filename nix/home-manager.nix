{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.invocateur;
  inherit (lib) mkEnableOption mkIf mkOption types;
  settings = {
    inherit (cfg) agent repositories multiplexer;
    github_enabled = cfg.githubEnabled;
    schedule_check_interval_seconds = cfg.scheduleCheckInterval;
    api_enabled = cfg.apiEnabled;
    api_listen = cfg.apiListen;
    bot_name = cfg.botName;
    use_hostname_as_bot_name = cfg.useHostnameAsBotName;
    workspace_name = cfg.workspaceName;
    tab_name = cfg.tabName;
    agent_args = cfg.agentArgs;
    prefix_github_comments = cfg.prefixGithubComments;
    acknowledgement_message = cfg.acknowledgementMessage;
    followup_acknowledgement_message = cfg.followupAcknowledgementMessage;
    repos_directory = cfg.reposDirectory;
    discover_repositories = cfg.discoverRepositories;
    discover_github_repositories = cfg.discoverGithubRepositories;
    clone_missing = cfg.cloneMissing;
    poll_interval_seconds = cfg.pollInterval;
    initial_lookback_seconds = cfg.initialLookback;
    review_comments = cfg.reviewComments;
    herdr_session = cfg.herdrSession;
    github_host = cfg.githubHost;
    gh_path =
      if cfg.githubEnabled
      then lib.getExe pkgs.gh
      else "gh";
    herdr_path = lib.getExe cfg.herdrPackage;
    git_path = lib.getExe pkgs.git;
    command_timeout_seconds = cfg.commandTimeout;
  };
  configFile = (pkgs.formats.toml {}).generate "invocateur.toml" settings;
in {
  options.services.invocateur = {
    enable = mkEnableOption "Invocateur triggers and scheduled tasks";
    githubEnabled = mkOption {
      type = types.bool;
      default = true;
      description = "Enable the GitHub comment trigger. Disable for schedules without GitHub.";
    };
    scheduleCheckInterval = mkOption {
      type = types.ints.positive;
      default = 1;
      description = "Seconds between checks of the dynamic schedule store, independent of GitHub pollInterval.";
    };
    apiEnabled = mkOption {
      type = types.bool;
      default = true;
      description = "Enable the local HTTP API used by agents and invo schedule for dynamic schedule CRUD.";
    };
    apiListen = mkOption {
      type = types.str;
      default = "127.0.0.1:7639";
      description = "Loopback IP address and port for the schedule API. Credentials are generated privately in the service state directory.";
    };
    multiplexer = mkOption {
      type = types.enum ["herdr"];
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
      default = "issue-{comment_id}";
      description = "Herdr tab name template, with the same placeholders as workspaceName.";
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
    botName = mkOption {
      type = types.str;
      default = "invo";
      example = "my-agent";
      description = "Bot name without @. Case-insensitive @name mentions outside quotes, code, and links start an agent. Ignored when useHostnameAsBotName is enabled.";
    };
    useHostnameAsBotName = mkEnableOption "use the machine's short system hostname as the bot name instead of botName";
    agent = mkOption {
      type = types.str;
      default = "codex";
      example = "claude";
      description = "Herdr-supported agent kind, such as codex or claude.";
    };
    agentArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Native arguments passed to herdr agent start after --.";
    };
    prefixGithubComments = mkOption {
      type = types.bool;
      default = true;
      description = "Instruct launched agents to prefix new GitHub comments with their agent instance name followed by a colon.";
    };
    acknowledgementMessage = mkOption {
      type = types.str;
      default = "🪄 [Invocateur](https://github.com/kosumic/invocateur) started {agent} for this task.\n\nTriggered by {comment_url}.";
      description = "Comment posted after successfully starting an agent. Supports {agent}, {agent_name}, {comment_url}, {repository}, {issue_number}, {multiplexer}, and {session}.";
    };
    followupAcknowledgementMessage = mkOption {
      type = types.str;
      default = "↪️ [Invocateur](https://github.com/kosumic/invocateur) forwarded this comment to {agent_name}.\n\nTriggered by {comment_url}.";
      description = "Comment posted after forwarding a follow-up to an existing agent. Supports the same placeholders as acknowledgementMessage.";
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
      description = "Shallow clone missing checkouts when a bot mention triggers.";
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
      description = "Agent and task command executables available to the service. Defaults to Codex. Herdr also needs its agent executable in the server's environment.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = pkgs.stdenv.hostPlatform.isLinux;
        message = "services.invocateur requires Linux systemd user services.";
      }
      {
        assertion = cfg.useHostnameAsBotName || builtins.match "[A-Za-z0-9_-]+" cfg.botName != null;
        message = "services.invocateur.botName must contain only letters, digits, underscores, or hyphens, without @.";
      }
    ];

    home.packages = [cfg.package cfg.herdrPackage];
    xdg.configFile."invocateur/config.toml".source = configFile;

    systemd.user.services.invocateur = {
      Unit.Description = "Run Invocateur triggers and scheduled tasks";
      Service = {
        ExecStart = "${lib.getExe cfg.package} serve --config ${configFile} --state-dir %S/invocateur";
        Restart = "on-failure";
        RestartSec = 15;
        StateDirectory = "invocateur";
        UMask = "0077";
        WorkingDirectory = config.home.homeDirectory;
        Environment = [
          "XDG_CONFIG_HOME=${config.xdg.configHome}"
          "PATH=${lib.makeBinPath ([pkgs.git] ++ lib.optional cfg.githubEnabled pkgs.gh ++ [cfg.herdrPackage] ++ cfg.extraPackages)}:${config.home.profileDirectory}/bin:${config.home.homeDirectory}/.local/bin:${config.home.homeDirectory}/.nix-profile/bin:/run/current-system/sw/bin"
        ];
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
