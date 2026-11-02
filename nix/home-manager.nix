{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.invocateur;
  inherit (lib) mkEnableOption mkIf mkOption types;
  needsAgents = cfg.githubEnabled || lib.any (task: task.enable && task.prompt != null) cfg.scheduledTasks;
  multiplexerPackage =
    if cfg.multiplexer == "herdr"
    then cfg.herdrPackage
    else if cfg.multiplexer == "tmux"
    then cfg.tmuxPackage
    else cfg.zellijPackage;
  settings = {
    inherit (cfg) agent repositories multiplexer;
    github_enabled = cfg.githubEnabled;
    schedule_check_interval_seconds = cfg.scheduleCheckInterval;
    scheduled_tasks = map (task:
      lib.filterAttrs (_: value: value != null) {
        inherit (task) name cron timezone directory prompt command;
        enabled = task.enable;
        every_seconds = task.everySeconds;
        run_on_startup = task.runOnStartup;
        timeout_seconds = task.commandTimeout;
        workspace_name = task.workspaceName;
        tab_name = task.tabName;
      })
    cfg.scheduledTasks;
    bot_name = cfg.botName;
    use_hostname_as_bot_name = cfg.useHostnameAsBotName;
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
    gh_path =
      if cfg.githubEnabled
      then lib.getExe pkgs.gh
      else "gh";
    herdr_path =
      if needsAgents && cfg.multiplexer == "herdr"
      then lib.getExe multiplexerPackage
      else "herdr";
    tmux_path =
      if needsAgents && cfg.multiplexer == "tmux"
      then lib.getExe multiplexerPackage
      else "tmux";
    zellij_path =
      if needsAgents && cfg.multiplexer == "zellij"
      then lib.getExe multiplexerPackage
      else "zellij";
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
      description = "Seconds between scheduled-task checks, independent of GitHub pollInterval.";
    };
    scheduledTasks = mkOption {
      default = [];
      description = "Machine-local cron or interval tasks. Each runs an agent prompt or command argv.";
      type = types.listOf (types.submodule {
        options = {
          name = mkOption {
            type = types.str;
            description = "Unique task identity, using 1–64 letters, digits, underscores, or hyphens.";
          };
          enable = mkOption {
            type = types.bool;
            default = true;
            description = "Enable this scheduled task.";
          };
          cron = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Cron expression; configure this or everySeconds.";
          };
          everySeconds = mkOption {
            type = types.nullOr types.ints.positive;
            default = null;
            description = "Interval between completed runs; configure this or cron.";
          };
          timezone = mkOption {
            type = types.str;
            default = "UTC";
            description = "IANA timezone for cron, such as America/New_York.";
          };
          directory = mkOption {
            type = types.str;
            default = cfg.reposDirectory;
            description = "Existing task working directory, absolute or starting with ~/.";
          };
          prompt = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Agent prompt, using the configured agent and multiplexer. Configure this or command.";
          };
          command = mkOption {
            type = types.listOf types.str;
            default = [];
            description = "Command and arguments, passed literally without a shell. Configure this or prompt.";
          };
          runOnStartup = mkOption {
            type = types.bool;
            default = false;
            description = "Run immediately the first time this task name is seen; restarts retain its schedule.";
          };
          commandTimeout = mkOption {
            type = types.nullOr types.ints.positive;
            default = null;
            description = "Command timeout, defaulting to the service commandTimeout.";
          };
          workspaceName = mkOption {
            type = types.str;
            default = "{task_name} {scheduled_at}";
            description = "Agent workspace template: {task_name}, {scheduled_at}, {agent_name}, {agent}, {multiplexer}, {session}.";
          };
          tabName = mkOption {
            type = types.str;
            default = "{agent_name}";
            description = "Agent tab/window template, with the same placeholders as workspaceName.";
          };
        };
      });
    };
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
      default = lib.optionals needsAgents [pkgs.codex];
      description = "Agent and task command executables available to the service. Defaults to Codex when agent tasks are enabled. Herdr also needs its agent executable in the server's environment.";
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

    home.packages = [cfg.package] ++ lib.optional needsAgents multiplexerPackage;
    xdg.configFile."invocateur/config.toml".source = configFile;

    systemd.user.services.invocateur = {
      Unit.Description = "Run Invocateur triggers and scheduled tasks";
      Service = {
        ExecStart = "${lib.getExe cfg.package} --config ${configFile} --state-dir %S/invocateur";
        Restart = "on-failure";
        RestartSec = 15;
        StateDirectory = "invocateur";
        UMask = "0077";
        WorkingDirectory = config.home.homeDirectory;
        Environment = [
          "XDG_CONFIG_HOME=${config.xdg.configHome}"
          "PATH=${lib.makeBinPath ([pkgs.git] ++ lib.optional cfg.githubEnabled pkgs.gh ++ lib.optional needsAgents multiplexerPackage ++ cfg.extraPackages)}:${config.home.profileDirectory}/bin:${config.home.homeDirectory}/.local/bin:${config.home.homeDirectory}/.nix-profile/bin:/run/current-system/sw/bin"
        ];
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
