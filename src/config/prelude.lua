-- hyprscope sandbox prelude.
--
-- This file defines a recording stand-in for Hyprland's `hl` table. The user's
-- real hyprland.lua runs against it inside hyprscope's own Lua state, so every
-- hl.window_rule() call is captured together with the file and line that made
-- it. Nothing here talks to the compositor and nothing is executed:
-- hl.exec_cmd, hl.on, hl.timer and hl.dispatch are inert.

local capture = { window_rules = {}, layer_rules = {}, workspace_rules = {}, errors = {}, calls = {} }

-- Frames of the Lua call stack above the hl.* function, innermost first.
local function stack()
  local frames = {}
  for level = 3, 40 do
    local info = debug.getinfo(level, "Sln")
    if not info then break end
    local src = info.source or ""
    if info.what ~= "C" and not src:find("hyprscope-prelude", 1, true) then
      if src:sub(1, 1) == "@" then src = src:sub(2) end
      frames[#frames + 1] = { file = src, line = info.currentline or 0, name = info.name }
    end
  end
  return frames
end

-- A value that tolerates any use: indexing yields another proxy, calling
-- yields a proxy, arithmetic yields 0, concatenation yields "". This is what
-- hl.dsp.* and the various handles return so configs never crash on them.
local proxy_mt = {}
local function proxy(name)
  return setmetatable({ __hs_proxy = name or "?" }, proxy_mt)
end
proxy_mt.__index = function(t, k)
  if k == "set_enabled" or k == "is_enabled" then
    return function() return true end
  end
  return proxy((rawget(t, "__hs_proxy") or "?") .. "." .. tostring(k))
end
proxy_mt.__call = function(t, ...) return proxy(rawget(t, "__hs_proxy")) end
proxy_mt.__concat = function(a, b)
  if type(a) == "table" then a = "" end
  if type(b) == "table" then b = "" end
  return a .. b
end
proxy_mt.__tostring = function(t) return "hl-proxy(" .. tostring(rawget(t, "__hs_proxy")) .. ")" end
proxy_mt.__add = function() return 0 end
proxy_mt.__sub = function() return 0 end
proxy_mt.__mul = function() return 0 end
proxy_mt.__div = function() return 0 end
proxy_mt.__unm = function() return 0 end
proxy_mt.__eq = function() return false end
proxy_mt.__lt = function() return false end
proxy_mt.__le = function() return false end
proxy_mt.__len = function() return 0 end

local function record(list, kind, tbl)
  local entry = { rule = tbl, stack = stack() }
  list[#list + 1] = entry
  capture.calls[#capture.calls + 1] = kind
  return entry
end

local hl = {}

function hl.window_rule(tbl)
  if type(tbl) ~= "table" then
    capture.errors[#capture.errors + 1] = { message = "hl.window_rule: argument must be a table", stack = stack() }
    return proxy("window_rule")
  end
  record(capture.window_rules, "window_rule", tbl)
  return proxy("window_rule")
end

function hl.layer_rule(tbl)
  if type(tbl) == "table" then record(capture.layer_rules, "layer_rule", tbl) end
  return proxy("layer_rule")
end

function hl.workspace_rule(tbl)
  if type(tbl) == "table" then record(capture.workspace_rules, "workspace_rule", tbl) end
  return proxy("workspace_rule")
end

-- Inert configuration entry points. They accept anything and do nothing.
local noop_names = {
  "config", "bind", "unbind", "monitor", "env", "animation", "curve", "device",
  "gesture", "layout", "permission", "plugin", "define_submap", "notification",
  "clear_crashed_lockscreen", "exec_scheduled_prop_refresh_immediately",
}
for _, name in ipairs(noop_names) do
  hl[name] = function(...) return proxy(name) end
end

-- Things that would have side effects in the compositor are swallowed.
function hl.exec_cmd(...) return nil end
function hl.dispatch(...) return nil end
function hl.on(event, fn) return proxy("on") end
function hl.timer(fn, opts) return proxy("timer") end

-- Live-state queries return empty or nil so `if w then` style code stays quiet.
function hl.get_windows() return {} end
function hl.get_workspaces() return {} end
function hl.get_monitors() return {} end
function hl.get_layers() return {} end
function hl.get_loaded_plugins() return {} end
function hl.get_workspace_windows() return {} end
function hl.get_active_window() return nil end
function hl.get_window() return nil end
function hl.get_urgent_window() return nil end
function hl.get_last_window() return nil end
function hl.get_workspace() return nil end
function hl.get_active_workspace() return nil end
function hl.get_active_special_workspace() return nil end
function hl.get_last_workspace() return nil end
function hl.get_monitor() return nil end
function hl.get_active_monitor() return nil end
function hl.get_monitor_at() return nil end
function hl.get_monitor_at_cursor() return nil end
function hl.get_cursor_pos() return { x = 0, y = 0 } end
function hl.get_current_submap() return "" end
function hl.get_config(key) return nil end
function hl.is_key_down() return false end
function hl.version() return __hs_hyprland_version or "0.0.0" end

hl.dsp = proxy("dsp")

_G.hl = hl
_G.__hs_capture = capture
