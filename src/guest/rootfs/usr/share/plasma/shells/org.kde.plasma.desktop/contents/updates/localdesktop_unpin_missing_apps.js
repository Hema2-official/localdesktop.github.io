// Local Desktop: unpin task manager launchers for applications that aren't installed, such as
// Discover in Plasma's default pins, which the Plasma preset leaves out. readConfig() can't see
// the widget's built-in defaults, so an unset list stands for this copy of them
// (plasma-desktop 6.7, applets/taskmanager/main.xml).
var defaultLaunchers = [
    "applications:systemsettings.desktop",
    "applications:org.kde.discover.desktop",
    "preferred://filemanager",
    "preferred://browser",
];

panels().forEach(function (panel) {
    panel.widgets().forEach(function (widget) {
        if (widget.type !== "org.kde.plasma.icontasks" && widget.type !== "org.kde.plasma.taskmanager") {
            return;
        }
        widget.currentConfigGroup = ["General"];
        var launchers = widget.readConfig("launchers", defaultLaunchers);
        if (typeof launchers === "string") {
            launchers = launchers ? launchers.split(",") : [];
        }
        var kept = launchers.filter(function (launcher) {
            var match = /^applications:(.+)$/.exec(launcher);
            return !match || applicationExists(match[1]);
        });
        if (kept.length !== launchers.length) {
            widget.writeConfig("launchers", kept);
            widget.reloadConfig();
        }
    });
});
