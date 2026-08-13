# Install Renium {{VERSION}}

## Windows

Double-click **Install Renium.cmd**. It works from inside the ZIP and after
extraction.

## macOS

Double-click **Install Renium.command**. If macOS blocks it, Control-click the
file, choose **Open**, then confirm.

## Linux

Double-click **install.sh** and choose **Run** if your file manager supports it.
Otherwise, open a terminal in the extracted folder and run:

```sh
./install.sh
```

The installer asks which detected editor should receive the Renium extension,
installs the Studio plugin on Windows and macOS, and puts `renium` on your PATH.
It uses the files from an extracted ZIP when available and verifies anything it
downloads. Restart the selected editor and Roblox Studio afterward.

Full documentation: https://github.com/Superwheat/renium/tree/v{{VERSION}}/tools/renium
