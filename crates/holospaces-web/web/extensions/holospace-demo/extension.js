const vscode = require("vscode");
function activate(context) {
  const item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  item.text = "HOLOSPACE-EXT-LIVE";
  item.show();
  context.subscriptions.push(item);
  context.subscriptions.push(
    vscode.commands.registerCommand("holospace.hello", () => vscode.window.showInformationMessage("hello from the holospace extension"))
  );
}
exports.activate = activate;
exports.deactivate = function () {};
