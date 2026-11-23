### App interface.

status-upload-complete = Przesyłanie ukończone ({ $count }). Przetworzenie przez YouTube Music może zająć minutę.
status-upload-failed = Przesyłanie nieudane: { $details }
status-upload-partial = Przesłano: { $count }. Błędy: { $details }
status-waiting-transfer = Oczekiwanie na zakończenie przenoszenia polubień…
status-liking-progress = Dodawanie polubień: { $done } z { $total }…
status-unliking-progress = Cofanie polubień: { $done } z { $total }…
status-playlist-created = Utworzono playlistę
status-new-playlist-tracks = Dodano do nowej playlisty: { $count }
status-playlist-added = Dodano do playlisty
status-playlist-updated = Zaktualizowano playlistę
status-playlist-sorted = Posortowano playlistę
status-playlist-sort-failed = Nie udało się posortować playlisty
status-playlist-move-failed = YouTube Music nie zapisał przeniesienia
status-library-saved = Zapisano w bibliotece
status-library-removed = Usunięto z biblioteki
status-unsubscribed = Anulowano subskrypcję
status-subscribe-failed = Nie udało się zasubskrybować
status-unsubscribe-failed = Nie udało się anulować subskrypcji
status-like-success = Dodano polubienia: { $count }
status-like-refused = YouTube Music nie przyjął zmiany. Spróbuj ponownie później.
status-refused-count = YouTube Music odrzucił: { $count }
status-removed-liked = Usunięto z polubionych utworów
status-play-premium = „{ $title }” wymaga YouTube Music Premium
status-play-sign-in = Zaloguj się, aby odtworzyć „{ $title }”
status-age-restricted = obowiązuje ograniczenie wiekowe
status-songs = { $count ->
    [one] { $count } utwór
    [few] { $count } utwory
    [many] { $count } utworów
   *[other] { $count } utworu
}
status-episodes = { $count ->
    [one] { $count } odcinek
    [few] { $count } odcinki
    [many] { $count } odcinków
   *[other] { $count } odcinka
}
