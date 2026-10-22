#!/usr/bin/env python3
"""Mark pngcrush 1.8.13 and skip PNG_IGNORE_ADLER32 when libpng removed it."""

import pathlib
import sys

NOTICE_ANCHOR = (
    " * If you have modified this source, you may insert additional notices\n"
    " * immediately after this sentence.\n"
)
NOTICE = (
    " *\n"
    " * Image Optimizer change: the PNG_IGNORE_ADLER32 call is wrapped in\n"
    " * #ifdef PNG_IGNORE_ADLER32 because libpng 1.6.32 removed that option.\n"
    " * This is not the original pngcrush 1.8.13 source.\n"
)
OLD_CALL = (
    "                    png_set_option(read_ptr, PNG_IGNORE_ADLER32,\n"
    "                        PNG_OPTION_ON);\n"
)
NEW_CALL = (
    "#ifdef PNG_IGNORE_ADLER32\n"
    "                    /* Removed from libpng 1.6.32+. Skip when the option is absent. */\n"
    "                    png_set_option(read_ptr, PNG_IGNORE_ADLER32,\n"
    "                        PNG_OPTION_ON);\n"
    "#endif\n"
)


def main() -> None:
    path = pathlib.Path(sys.argv[1])
    text = path.read_text()
    if "Image Optimizer change:" not in text:
        if NOTICE_ANCHOR not in text:
            raise SystemExit("pngcrush notice anchor not found")
        text = text.replace(NOTICE_ANCHOR, NOTICE_ANCHOR + NOTICE, 1)
    if "Removed from libpng 1.6.32" not in text:
        if OLD_CALL not in text:
            raise SystemExit("pngcrush.c patch context not found")
        text = text.replace(OLD_CALL, NEW_CALL, 1)
    path.write_text(text)


if __name__ == "__main__":
    main()
