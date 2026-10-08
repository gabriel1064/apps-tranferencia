-- Clean desktop toggle for the niri-live-reflow fork.
-- State comes from niri's event stream: workspaces carry `is_desktop_shown`.

local workspaces = {}
local shown = nil

local function outputName()
  if not barWidget.outputName then
    return nil
  end
  local ok, name = pcall(barWidget.outputName)
  if ok then
    return name
  end
  return nil
end

local function render()
  barWidget.setText("")
  if shown then
    barWidget.setGlyph("wallpaper")
    barWidget.setGlyphColor("primary")
    barWidget.setTooltip("Desktop limpo: ligado - clique para trazer as janelas de volta")
  else
    barWidget.setGlyph("app-window")
    barWidget.setGlyphColor("on_surface")
    barWidget.setTooltip("Desktop limpo: desligado - clique para esconder as janelas")
  end
end

local function recompute()
  local out = outputName()
  local active = nil
  for _, ws in pairs(workspaces) do
    if out ~= nil and ws.output == out and ws.is_active then
      active = ws
    end
  end
  if active == nil then
    for _, ws in pairs(workspaces) do
      if ws.is_focused then
        active = ws
      end
    end
  end
  local now = active ~= nil and active.is_desktop_shown == true
  if now ~= shown then
    shown = now
    render()
  end
end

local function replace(list)
  workspaces = {}
  for _, ws in ipairs(list or {}) do
    workspaces[ws.id] = ws
  end
  recompute()
end

local function activated(id, focused)
  local target = workspaces[id]
  if target == nil then
    return
  end
  for _, ws in pairs(workspaces) do
    if ws.output == target.output then
      ws.is_active = ws.id == id
    end
    if focused then
      ws.is_focused = ws.id == id
    end
  end
  recompute()
end

local function onEventLine(line)
  local ok, event = pcall(noctalia.json.decode, line)
  if not ok or type(event) ~= "table" then
    return
  end
  if event.WorkspacesChanged then
    replace(event.WorkspacesChanged.workspaces)
  elseif event.WorkspaceActivated then
    activated(event.WorkspaceActivated.id, event.WorkspaceActivated.focused)
  end
end

function update()
  noctalia.setUpdateInterval(5000)
  render()
end

function onClick()
  noctalia.runAsync("niri-live-reflow msg action toggle-show-desktop")
end

function onRightClick()
  noctalia.runAsync("niri-live-reflow msg action toggle-show-desktop")
end

shown = false
render()
if not noctalia.runStream("while :; do niri-live-reflow msg -j event-stream; sleep 2; done", onEventLine) then
  noctalia.log("clean-desktop: unable to subscribe to niri event-stream")
end
