"""Tiny translation table: English source strings, Italian translations.

The app is small enough that a dictionary beats a gettext toolchain.
"""
import os

_IT = {
    # Window
    "Ready": "Pronto",
    "Connecting…": "Collegamento…",
    "Connected": "Connesso",
    # What the tablet is connected by (plusone/link/paths.py)
    "{state} · {path}": "{state} · {path}",
    "{detail} · {path}": "{detail} · {path}",
    "cable": "cavo",
    "Wi-Fi": "Wi-Fi",
    "Wi-Fi Direct": "Wi-Fi Direct",
    "Bluetooth": "Bluetooth",
    "the internet": "internet",
    "reconnecting…": "riconnessione…",
    "Use the cable": "Usa il cavo",
    "The tablet plugged in goes by the cable, the fastest way; its file transfer is off meanwhile":
        "Il tablet collegato passa dal cavo, la via più veloce; il suo trasferimento file resta spento intanto",
    "Problem": "Problema",
    "Connect": "Collega",
    "Disconnect": "Scollega",
    "Resolution": "Risoluzione",
    "Automatic": "Automatica",
    "Quality": "Qualità",
    "Touch and S Pen": "Touch e S Pen",
    "Sign on Tablet": "Firma sul tablet",
    "Ready in the background": "Pronto in background",
    "Starts at login and stays ready after you close this window":
        "Parte all'accesso e resta pronto anche dopo aver chiuso questa finestra",
    "Quick Settings toggle": "Interruttore nelle impostazioni rapide",
    "Adds a “Tablet” button to GNOME's Quick Settings":
        "Aggiunge un pulsante «Tablet» alle impostazioni rapide di GNOME",
    "Log": "Registro",
    # Notifications and background
    "Tablet connected": "Tablet collegato",
    "Keeps the Quick Settings toggle ready to connect the tablet":
        "Tiene pronto l'interruttore nelle impostazioni rapide per collegare il tablet",
    # Signing
    "Cancel": "Annulla",
    "Done": "Fatto",
    "Undo": "Annulla",
    "Clear page": "Pulisci pagina",
    "My signature": "La mia firma",
    "My signatures": "Le mie firme",
    "Select": "Seleziona",
    "Drag it, turn it, or resize from a corner": "Trascina, ruota o ridimensiona dagli angoli",
    "Delete": "Elimina",
    "Done with the selection": "Fine selezione",
    "Placed with one tap when signing, here and on the tablet":
        "Si mettono con un tocco quando firmi, qui e sul tablet",
    "New signature": "Nuova firma",
    "Sign here with the pen or the mouse": "Firma qui con la penna o il mouse",
    "Start over": "Ricomincia",
    "Save": "Salva",
    "No signature saved yet": "Ancora nessuna firma salvata",
    "Place this signature": "Metti questa firma",
    "Remove": "Rimuovi",
    "Remove this signature?": "Rimuovere questa firma?",
    "It goes from the tablet too, the next time you sign there.":
        "Sparisce anche dal tablet, la prossima volta che firmi lì.",
    "Keep as my signature": "Salva come mia firma",
    "Pen": "Penna",
    "Eraser": "Gomma",
    "Previous page": "Pagina precedente",
    "Next page": "Pagina successiva",
    "Write with the pen · hold its button to erase": "Scrivi con la penna · tieni premuto il tasto per cancellare",
    "Thickness": "Spessore",
    "Stabilization": "Stabilizzazione",
    "Blue ink": "Inchiostro blu",
    "Black": "Nero",
    "Blue": "Blu",
    "Red": "Rosso",
    "Green": "Verde",
    "Date": "Data",
    "Text": "Testo",
    "Fit": "Adatta",
    "Tap where your signature goes": "Tocca dove va la firma",
    "Saved: next time, “My signature” places it with one tap":
        "Salvata: la prossima volta «La mia firma» la mette con un tocco",
    "Tap where the date goes": "Tocca dove va la data",
    "Text to add": "Testo da aggiungere",
    "Place": "Inserisci",
    "Tap where the text goes": "Tocca dove va il testo",
    "Could not save: {error}": "Impossibile salvare: {error}",
    "Discard the signature?": "Scartare la firma?",
    "Choose where to save the signed copy": "Scegli dove salvare la copia firmata",
    "Save the signed PDF": "Salva il PDF firmato",
    "Keep signing": "Continua a firmare",
    "Discard": "Scarta",
    "Sign on the tablet": "Firma sul tablet",
    "Sign on this PC": "Firma su questo PC",
    "Cannot open {name}": "Impossibile aprire {name}",
    "Cannot sign {name}": "Impossibile firmare {name}",
    "{name} is not a file": "{name} non è un file",
    "{name} is too big to sign: more than {size} MB": "{name} è troppo grande da firmare: più di {size} MB",
    "{name} is not a PDF": "{name} non è un PDF",
    "{name} cannot be read: {error}": "{name} non si può leggere: {error}",
    "{name} needs a password to open: unlock it first": "{name} richiede una password: sbloccalo prima",
    "Signed": "Firmato",
    "Open": "Apri",
    "Show in folder": "Mostra nella cartella",
    "Choose the PDFs to sign": "Scegli i PDF da firmare",
    # Engine states
    "Sending {name} to Plus One on {tablet}": "Invio di {name} a Plus One su {tablet}",
    "Not paired": "Non abbinato",
    "Sign {name} on {tablet}": "Firma {name} su {tablet}",
    "{count} PDFs to sign on {tablet}": "{count} PDF da firmare su {tablet}",
    "It had a digital signature: in the signed copy, it no longer holds":
        "Aveva una firma digitale: nella copia firmata non vale più",
    "Forget": "Dimentica",
    "Paired tablets": "Tablet abbinati",
    "Forget {tablet}?": "Dimenticare {tablet}?",
    "Signed {count} PDFs": "{count} PDF firmati",
    "Lost the tablet": "Tablet perso",
    "What was signed on the tablet did not reach the PC: {name} is unchanged":
        "Quanto firmato sul tablet non è arrivato al PC: {name} è rimasto com'era",
    "Opening the desktop on {name}…": "Apro il desktop su {name}…",
    "Tablet speakers": "Casse del tablet",
    # The window's sidebar and its pages
    "Microphone": "Microfono",
    "Speakers": "Casse",
    "Tablets": "Tablet",
    "Not found": "Non trovato",
    "Touch and pen": "Tocco e penna",
    "Fingers and the pen drive the desktop; the pen with pressure and tilt":
        "Dita e penna comandano il desktop; la penna con pressione e inclinazione",
    "Or right-click a PDF in Files: Sign on Tablet": "Oppure tasto destro su un PDF in File: Firma sul tablet",
    "Plus One's app, for Android 13 or later": "L'app Plus One, per Android 13 o successivi",
    "The tablet's camera as a webcam that every program on this PC sees; the tablet turns it on only while one uses it.":
        "La fotocamera del tablet come webcam che ogni programma del PC vede; il tablet la accende solo mentre "
        "uno la usa.",
    "The tablet's microphone for this PC's programs; the tablet turns it on only while one records.":
        "Il microfono del tablet per i programmi del PC; il tablet lo accende solo mentre uno registra.",
    "The tablet's speakers as a sound output of this PC, there while the tablet is: the sound as it is, uncompressed.":
        "Le casse del tablet come uscita audio del PC, presenti finché c'è il tablet: il suono così com'è, "
        "senza compressione.",
    "What Plus One did, to send along with a problem": "Cosa ha fatto Plus One, da allegare a un problema",
    "Needs PipeWire's pactl and pw-link, and python3-cryptography":
        "Servono pactl e pw-link di PipeWire, e python3-cryptography",
    "As a sound output for this PC's programs": "Come uscita audio per i programmi di questo PC",
    "On: choose “Tablet speakers” as the output in the Sound settings":
        "Attive: scegli «Tablet speakers» come uscita nelle impostazioni Audio",
    "Playing {programs}": "Suona {programs}",
    "Playing": "Suona",
    "A program plays on the speakers, but the tablet is not reachable":
        "Un programma suona sulle casse, ma il tablet non è raggiungibile",
    "Allow the speakers on the tablet": "Consenti le casse sul tablet",
    "{width} × {height}, sharp: as {half}": "{width} × {height}, nitido: come {half}",
    "Above 1920 × 1200 the picture comes a little later": "Sopra 1920 × 1200 l'immagine arriva un po' più tardi",
    "The tablet's sizes appear after it showed the desktop once":
        "Le misure del tablet compaiono dopo che ha mostrato il desktop una volta",
    "{name}: paused while Plus One is not on its screen": "{name}: in pausa finché Plus One non è sul suo schermo",
    "The tablet closed the desktop": "Il tablet ha chiuso il desktop",
    "The tablet is no longer reachable": "Il tablet non è più raggiungibile",
    # 0.9: the app only
    "Needs python3-cryptography": "Serve python3-cryptography",
    "On {name}, let Plus One display over other apps, or open it":
        "Su {name}, consenti a Plus One di apparire sopra le altre app, oppure aprilo",
    "Plus One on the tablet is another version than on this PC: it is updated once nothing is in use; or sign here":
        "Plus One sul tablet è un'altra versione rispetto a questo PC: si aggiorna appena niente è in uso; "
        "oppure firma qui",
    "Plus One on {tablet} did not answer: open it there, or sign here":
        "Plus One su {tablet} non ha risposto: aprilo lì, oppure firma qui",
    "Plus One on {tablet} is not on the network: open it there, or sign here":
        "Plus One su {tablet} non è in rete: aprilo lì, oppure firma qui",
    "The second screen ended": "Il secondo schermo si è chiuso",
    "{name} did not answer: is Plus One open on it?": "{name} non ha risposto: Plus One è aperto lì?",
    "{name} did not let the desktop show: {reason}": "{name} non ha mostrato il desktop: {reason}",
    "{name} is not on the network: open Plus One on it": "{name} non è in rete: apri Plus One lì",
    # 0.8: updates
    "Updating Plus One on {tablet}": "Aggiornamento di Plus One su {tablet}",
    "To {version}, as on this PC": "Alla {version}, come su questo PC",
    "Plus One {version} is installed": "Plus One {version} è installato",
    "It starts again by itself once nothing is in use": "Si riavvia da solo appena niente è in uso",
    "Restart now": "Riavvia ora",
    "Plus One updated to {version}": "Plus One aggiornato alla {version}",
    "From {before}": "Dalla {before}",
    "What's new": "Novità",
    "Keep the tablet's app up to date": "Tieni aggiornata l'app sul tablet",
    "With this PC's version, while nothing is in use": "Alla versione di questo PC, quando niente è in uso",
    "The desktop stopped: {error}": "Il desktop si è fermato: {error}",
    "Connecting to {name}…": "Collegamento a {name}…",
    # 0.6: pairing from the window, the tablet in use
    "About Plus One": "Informazioni su Plus One",
    "Pair a tablet": "Abbina un tablet",
    "No tablet": "Nessun tablet",
    "Choose the tablet": "Scegli il tablet",
    "Use as second screen": "Usa come secondo schermo",
    "Stop": "Interrompi",
    "Sign a PDF…": "Firma un PDF…",
    "Second screen · {width} × {height} at {fps} Hz": "Secondo schermo · {width} × {height} a {fps} Hz",
    "Tablet camera": "Fotocamera del tablet",
    "Tablet microphone": "Microfono del tablet",
    "Needs the v4l2loopback module: see the README": "Serve il modulo v4l2loopback: vedi il README",
    "Needs PipeWire's pactl and pw-link": "Servono pactl e pw-link di PipeWire",
    "Needs an H.264 decoder: gstreamer1.0-libav": "Serve un decodificatore H.264: gstreamer1.0-libav",
    "The webcam module is loaded with other settings: sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback":
        "Il modulo della webcam è caricato con altre impostazioni: sudo modprobe -r v4l2loopback && "
        "sudo modprobe v4l2loopback",
    "Needs WebRTC for GStreamer: gstreamer1.0-nice and gir1.2-gst-plugins-bad-1.0":
        "Serve WebRTC per GStreamer: gstreamer1.0-nice e gir1.2-gst-plugins-bad-1.0",
    "As a webcam for this PC's programs": "Come webcam per i programmi di questo PC",
    "As a microphone for this PC's programs": "Come microfono per i programmi di questo PC",
    "On: the tablet turns its camera on while a program uses it":
        "Attiva: il tablet accende la fotocamera mentre un programma la usa",
    "On: the tablet turns its microphone on while a program uses it":
        "Attivo: il tablet accende il microfono mentre un programma lo usa",
    "In use": "In uso",
    "A program uses the camera, but the tablet is not reachable":
        "Un programma usa la fotocamera, ma il tablet non è raggiungibile",
    "A program uses the microphone, but the tablet is not reachable":
        "Un programma usa il microfono, ma il tablet non è raggiungibile",
    "Not allowed on the tablet: Plus One → For the PC": "Non consentito sul tablet: Plus One → Per il PC",
    "On the tablet, allow Plus One to use the camera": "Sul tablet, consenti a Plus One di usare la fotocamera",
    "On the tablet, allow Plus One to use the microphone": "Sul tablet, consenti a Plus One di usare il microfono",
    "On the tablet, let Plus One display over other apps":
        "Sul tablet, consenti a Plus One di comparire sopra le altre app",
    "On the tablet, another app is using the camera": "Sul tablet un'altra app sta usando la fotocamera",
    "On the tablet, another app is using the microphone": "Sul tablet un'altra app sta usando il microfono",
    "Camera": "Fotocamera",
    "Noise reduction": "Riduzione del rumore",
    "Off": "Spenta",
    "Light, on the tablet": "Leggera, sul tablet",
    "Neural, on this PC": "Neurale, su questo PC",
    "Neural takes out more of the room's noise, with about half a processor core":
        "La neurale toglie più rumore della stanza, con circa mezzo core del processore",
    "Neural needs DeepFilterNet's plugin in ~/.ladspa: light until then":
        "La neurale richiede il plugin di DeepFilterNet in ~/.ladspa: fino ad allora, leggera",
    "Front": "Frontale",
    "Back": "Posteriore",
    "Picture": "Immagine",
    "1080p takes about twice the bandwidth. A change waits until no program uses the camera":
        "1080p usa circa il doppio della banda. Un cambio attende che nessun programma usi la fotocamera",
    "Pair another tablet…": "Abbina un altro tablet…",
    "With Plus One on your Wi-Fi": "Con Plus One sulla tua Wi-Fi",
    "Not there? Open Plus One on the tablet, on the same Wi-Fi as this PC.":
        "Non c'è? Apri Plus One sul tablet, sulla stessa Wi-Fi di questo PC.",
    "Looking for tablets…": "Cerco i tablet…",
    "Paired": "Abbinato",
    "Pair only if the tablet shows this same code. Confirm there too.":
        "Abbina solo se il tablet mostra lo stesso codice. Conferma anche lì.",
    "Same code": "Il codice è uguale",
    "Confirm on the tablet…": "Conferma sul tablet…",
    "Paired with {name}": "Abbinato a {name}",
    "Not paired: the code was not confirmed on both screens":
        "Non abbinato: il codice non è stato confermato su entrambi gli schermi",
    "{name} did not answer": "{name} non ha risposto",
    "Try again": "Riprova",
    "Allow the camera on the tablet": "Consenti la fotocamera sul tablet",
    "Allow the microphone on the tablet": "Consenti il microfono sul tablet",
    "A program wants it: tap Plus One's notification on the tablet": "Un programma del PC ne ha bisogno: tocca la notifica di Plus One sul tablet",
    "Add a “Tablet” button to Quick Settings?": "Aggiungere un pulsante «Tablet» alle impostazioni rapide?",
    "Add": "Aggiungi",
    "Balanced": "Equilibrata",
    "Sharper text": "Testo più nitido",
    "For weak Wi-Fi": "Per Wi-Fi debole",
    "Sharper text takes more of the Wi-Fi": "Il testo più nitido usa più Wi-Fi",
    "Second screen": "Secondo schermo",
    "Without Plus One's pen helper (the .deb installs it) the S Pen works as a mouse, without pressure":
        "Senza l'aiutante di Plus One per la penna (lo installa il .deb) la S Pen funziona come un mouse, "
        "senza pressione",
    "In use by {programs}": "In uso da {programs}",
    "Install Plus One on the tablet too, then pair it to use it as a second screen, to sign PDFs and as a webcam.":
        "Installa Plus One anche sul tablet, poi abbinalo per usarlo come secondo schermo, per firmare i PDF "
        "e come webcam.",
    "Get the tablet app": "Scarica l'app per il tablet",
    "Not found: open Plus One on the tablet": "Non trovato: apri Plus One sul tablet",
    "No tablet found yet. Open Plus One on the tablet, on the same Wi-Fi as this PC.":
        "Ancora nessun tablet. Apri Plus One sul tablet, sulla stessa Wi-Fi di questo PC.",
    "The one in use gets the desktop and the PDFs. Forget one you no longer use.":
        "Quello in uso riceve lo schermo e i PDF. Dimentica quelli che non usi più.",
    "No tablet paired": "Nessun tablet abbinato",
    "To use it again, pair it again: both screens show a code.":
        "Per usarlo di nuovo, abbinalo di nuovo: entrambi gli schermi mostreranno un codice.",
    "Signing": "Firma",
    "General": "Generale",
    "In Files and Open With, for PDFs": "In File e Apri con, per i PDF",
    "Copy": "Copia",
    "Your Galaxy Tab as a second monitor, and to sign PDFs with its S Pen":
        "Il tuo Galaxy Tab come secondo monitor, e per firmare i PDF con la S Pen",
    "Pair your tablet in Plus One to sign {name} on it": "Abbina il tuo tablet in Plus One per firmarci {name}",
    "{name} no longer knows this PC: pair it again": "{name} non riconosce più questo PC: abbinalo di nuovo",
    "The tablet no longer knows this PC: pair it again": "Il tablet non riconosce più questo PC: abbinalo di nuovo",
    "Plus One on the tablet and on this PC are different versions: update them":
        "Plus One sul tablet e su questo PC sono versioni diverse: aggiornali",
    "Pair a tablet first": "Prima abbina un tablet",
    "Plus One on {name} and on this PC are different versions: update them":
        "Plus One su {name} e su questo PC sono versioni diverse: aggiornali",
}

# Templates that read the same in every language.
NEUTRAL = {"{error}", "{name} · {width}×{height} @ {fps} Hz"}

_current = "en"


def system_language():
    for var in ("LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"):
        value = os.environ.get(var)
        if value:
            return "it" if value.startswith("it") else "en"
    return "en"


def set_language(code):
    global _current
    _current = system_language() if code == "auto" else code


def _(text):
    return _IT.get(text, text) if _current == "it" else text
