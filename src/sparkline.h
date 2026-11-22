// <sparkline>: a price's last week as a line, drawn by the element itself
// (RmlUi has no lines or paths). See sparkline.cpp for its attributes.
#pragma once

// Registers the element with RmlUi: after Rml::Initialise(), before any
// document that uses it is loaded.
void register_sparkline();
