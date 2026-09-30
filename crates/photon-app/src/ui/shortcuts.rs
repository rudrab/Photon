//! Keyboard Shortcuts window (Ctrl+?), listing the keys the timeline, the
//! viewer and the window actually handle.

#![allow(deprecated)] // GtkShortcutsWindow: still GNOME's standard until libadwaita 1.8

use gtk4::prelude::*;

const UI: &str = r#"
<interface>
  <object class="GtkShortcutsWindow" id="window">
    <property name="modal">1</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">main</property>
        <property name="max-height">12</property>

        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Import from folder</property>
              <property name="accelerator">&lt;ctrl&gt;o</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Slideshow</property>
              <property name="accelerator">F5</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Undo</property>
              <property name="accelerator">&lt;ctrl&gt;z</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Redo</property>
              <property name="accelerator">&lt;ctrl&gt;&lt;shift&gt;z</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Refresh library</property>
              <property name="accelerator">&lt;ctrl&gt;r</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Search</property>
              <property name="accelerator">&lt;ctrl&gt;f</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Preferences</property>
              <property name="accelerator">&lt;ctrl&gt;comma</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Keyboard shortcuts</property>
              <property name="accelerator">&lt;ctrl&gt;question</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Quit</property>
              <property name="accelerator">&lt;ctrl&gt;q</property></object></child>
          </object>
        </child>

        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Timeline</property>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Move between photos</property>
              <property name="accelerator">Left Right Up Down</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">First / last photo</property>
              <property name="accelerator">Home End</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Open photo</property>
              <property name="accelerator">Return</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Bigger / smaller / default thumbnails</property>
              <property name="accelerator">&lt;ctrl&gt;plus &lt;ctrl&gt;minus &lt;ctrl&gt;0</property></object></child>
          </object>
        </child>

        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Selection</property>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Extend selection</property>
              <property name="accelerator">&lt;shift&gt;Left &lt;shift&gt;Right &lt;shift&gt;Up &lt;shift&gt;Down</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Select or deselect photo</property>
              <property name="accelerator">space</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Select all</property>
              <property name="accelerator">&lt;ctrl&gt;a</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Clear selection / stop selecting</property>
              <property name="accelerator">Escape</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Copy (share)</property>
              <property name="accelerator">&lt;ctrl&gt;c</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Export</property>
              <property name="accelerator">&lt;ctrl&gt;e</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Move to Trash</property>
              <property name="accelerator">Delete</property></object></child>
          </object>
        </child>

        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Culling (timeline and viewer)</property>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Rate 1–5 stars</property>
              <property name="accelerator">1...5</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Clear rating</property>
              <property name="accelerator">0</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Pick / reject / unflag</property>
              <property name="accelerator">p x u</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Rotate left / right</property>
              <property name="accelerator">bracketleft bracketright &lt;ctrl&gt;r</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Hold Shift to also go to the next photo</property>
              <property name="accelerator">&lt;shift&gt;1...5 &lt;shift&gt;p</property></object></child>
          </object>
        </child>

        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Viewer</property>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Previous / next photo</property>
              <property name="accelerator">Left Right</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Back to timeline</property>
              <property name="accelerator">Escape</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Show details</property>
              <property name="accelerator">i</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Switch between a shot's files (RAW / JPG)</property>
              <property name="accelerator">v</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Toggle Fit / 1:1 zoom</property>
              <property name="accelerator">z</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Zoom in / out (also Ctrl+scroll, pinch)</property>
              <property name="accelerator">plus minus</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Zoom to fit / 100%</property>
              <property name="accelerator">&lt;ctrl&gt;0 &lt;ctrl&gt;1</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Zoom to next / previous face (also Compare, Survey)</property>
              <property name="accelerator">f &lt;shift&gt;f</property></object></child>
            <child><object class="GtkShortcutsShortcut">
              <property name="title">Copy / export photo</property>
              <property name="accelerator">&lt;ctrl&gt;c &lt;ctrl&gt;e</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>
"#;

pub fn show(parent: &impl IsA<gtk4::Window>) {
    let builder = gtk4::Builder::from_string(UI);
    let window: gtk4::ShortcutsWindow = builder.object("window").expect("shortcuts window");
    window.set_transient_for(Some(parent));
    window.present();
}
