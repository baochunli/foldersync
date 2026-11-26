on run argv
    set operation to item 1 of argv
    with timeout of 10 seconds
        if operation is "open" then
            set fixturePath to item 2 of argv
            set fixtureFolder to POSIX file fixturePath as alias
            set viewName to item 3 of argv
            tell application id "com.apple.finder"
                set fixtureWindow to make new Finder window to fixtureFolder
                try
                    if viewName is "list" then
                        set current view of fixtureWindow to list view
                    else
                        set current view of fixtureWindow to icon view
                    end if
                    set index of fixtureWindow to 1
                    activate
                    if POSIX path of ((target of fixtureWindow) as alias) is not fixturePath & "/" then
                        error "Finder did not open the requested fixture directory"
                    end if
                    return id of fixtureWindow
                on error messageText number errorNumber
                    close fixtureWindow
                    error messageText number errorNumber
                end try
            end tell
        else if operation is "close" then
            set fixtureID to item 2 of argv as integer
            set fixturePath to item 3 of argv
            tell application id "com.apple.finder"
                if exists Finder window id fixtureID then
                    set fixtureWindow to Finder window id fixtureID
                    if POSIX path of ((target of fixtureWindow) as alias) is not fixturePath & "/" then
                        error "Finder window target changed"
                    end if
                    close fixtureWindow
                end if
            end tell
        else
            error "Unknown Finder window operation"
        end if
    end timeout
end run
